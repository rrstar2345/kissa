// Package session owns the per-client, in-memory broker sessions. A session
// holds the broker connection (and with it the credentials) from the moment a
// client opens it until the client ends it or it expires. Nothing is persisted.
package session

import (
	"context"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"brokerproxy/internal/broker"
	"brokerproxy/internal/model"
)

type Session struct {
	ID        string
	ClientID  string // the proxy client (API key) that owns the session
	Broker    string
	CreatedAt time.Time
	Conn      broker.Conn
	Hub       *Hub

	lastUsed  atomic.Int64
	closeOnce sync.Once

	idemMu sync.Mutex
	idem   map[string]*idemEntry
}

type idemEntry struct {
	done    chan struct{}
	ack     *model.OrderAck
	unknown bool // outcome ambiguous (e.g. timeout): the order may exist at the broker
}

func (s *Session) Touch()              { s.lastUsed.Store(time.Now().UnixNano()) }
func (s *Session) LastUsed() time.Time { return time.Unix(0, s.lastUsed.Load()) }

// Close ends the session: stops the broker feed, drops the connection (and
// credentials) and notifies attached WebSockets.
func (s *Session) Close(reason string) {
	s.closeOnce.Do(func() {
		_ = s.Conn.Close()
		s.Hub.Close(reason)
		s.idemMu.Lock()
		s.idem = nil
		s.idemMu.Unlock()
	})
}

// PlaceOnce places an order. When the order carries a correlationId it is
// treated as an idempotency key for the session: a retry of an order that was
// already accepted returns the original acknowledgement (replay=true) and
// never reaches the broker twice, even when requests race. If an earlier
// attempt's outcome is unknown (timeout / broker unreachable) the retry is
// refused with 409 so the client can look the order up by correlationId.
func (s *Session) PlaceOnce(ctx context.Context, req *model.OrderRequest) (ack *model.OrderAck, replay bool, err error) {
	cid := req.CorrelationID
	if cid == "" {
		ack, err = s.Conn.PlaceOrder(ctx, req)
		return ack, false, err
	}
	for {
		s.idemMu.Lock()
		if s.idem == nil {
			s.idemMu.Unlock()
			return nil, false, model.Errf(http.StatusGone, model.CodeNotFound, "session ended")
		}
		e, exists := s.idem[cid]
		if !exists {
			e = &idemEntry{done: make(chan struct{})}
			s.idem[cid] = e
			s.idemMu.Unlock()
			break
		}
		s.idemMu.Unlock()
		select {
		case <-e.done:
		case <-ctx.Done():
			return nil, false, ctx.Err()
		}
		switch {
		case e.unknown:
			return nil, false, model.Errf(http.StatusConflict, model.CodeConflict,
				"an earlier attempt with correlationId %q has an unknown outcome; query GET /v1/orders/external/%s before retrying", cid, cid)
		case e.ack != nil:
			return e.ack, true, nil
		}
		// owner failed definitively and removed the entry: try to become the owner
	}

	ack, err = s.Conn.PlaceOrder(ctx, req)

	s.idemMu.Lock()
	e := s.idem[cid]
	if e != nil {
		switch {
		case err == nil:
			e.ack = ack
		case definiteFailure(err):
			delete(s.idem, cid)
		default:
			e.unknown = true
		}
		close(e.done)
	}
	s.idemMu.Unlock()
	return ack, false, err
}

// definiteFailure reports whether err proves the broker did not accept the order.
func definiteFailure(err error) bool {
	ae, ok := err.(*model.APIError)
	if !ok {
		return false
	}
	switch ae.Code {
	case model.CodeInvalidRequest, model.CodeMarketNotAllowed, model.CodeRateLimited,
		model.CodeBrokerAuth, model.CodeBrokerRejected:
		return true
	}
	return false
}
