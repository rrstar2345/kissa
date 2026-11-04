package dhan

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"time"

	"github.com/gorilla/websocket"

	"brokerproxy/internal/broker"
	"brokerproxy/internal/model"
)

const (
	pingEvery   = 30 * time.Second
	readTimeout = 5 * time.Minute // refreshed by any message or pong
	maxBackoff  = 30 * time.Second
)

// feed maintains one upstream Dhan live-order WebSocket for one account and
// turns each order_alert into a normalized model.Event.
type feed struct {
	url      string
	clientID string
	token    string
	sink     broker.Sink
}

// run connects, reads and reconnects with backoff until ctx is cancelled.
func (f *feed) run(ctx context.Context) {
	backoff := time.Second
	for ctx.Err() == nil {
		started := time.Now()
		err := f.once(ctx)
		if ctx.Err() != nil {
			return
		}
		detail := "connection closed"
		if err != nil {
			detail = err.Error()
		}
		slog.Warn("dhan order feed disconnected", "client", f.clientID, "reason", detail)
		f.status("disconnected", detail)
		if time.Since(started) > time.Minute {
			backoff = time.Second
		}
		select {
		case <-time.After(backoff):
		case <-ctx.Done():
			return
		}
		if backoff *= 2; backoff > maxBackoff {
			backoff = maxBackoff
		}
	}
}

func (f *feed) status(state, detail string) {
	f.sink(model.Event{Type: model.EventFeedStatus, Broker: Name, State: state, Detail: detail})
}

func (f *feed) once(ctx context.Context) error {
	d := websocket.Dialer{HandshakeTimeout: 15 * time.Second, Proxy: http.ProxyFromEnvironment}
	c, _, err := d.DialContext(ctx, f.url, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.Close()

	login := map[string]any{
		"LoginReq": map[string]any{"MsgCode": 42, "ClientId": f.clientID, "Token": f.token},
		"UserType": "SELF",
	}
	if err := c.WriteJSON(login); err != nil {
		return fmt.Errorf("login: %w", err)
	}
	f.status("connected", "login sent")

	c.SetReadLimit(1 << 20)
	_ = c.SetReadDeadline(time.Now().Add(readTimeout))
	c.SetPongHandler(func(string) error { return c.SetReadDeadline(time.Now().Add(readTimeout)) })

	stop := make(chan struct{})
	defer close(stop)
	go func() { // keepalive; also unblocks the read loop when ctx is cancelled
		t := time.NewTicker(pingEvery)
		defer t.Stop()
		for {
			select {
			case <-t.C:
				_ = c.WriteControl(websocket.PingMessage, nil, time.Now().Add(5*time.Second))
			case <-ctx.Done():
				c.Close()
				return
			case <-stop:
				return
			}
		}
	}()

	for {
		_, msg, err := c.ReadMessage()
		if err != nil {
			return err
		}
		_ = c.SetReadDeadline(time.Now().Add(readTimeout))
		f.handle(msg)
	}
}

func (f *feed) handle(msg []byte) {
	var env struct {
		Type string          `json:"Type"`
		Data json.RawMessage `json:"Data"`
	}
	if err := json.Unmarshal(msg, &env); err != nil || env.Type != "order_alert" {
		return // acks / unknown frames are ignored
	}
	ev := model.Event{Type: model.EventOrderUpdate, Broker: Name, Raw: append(json.RawMessage(nil), env.Data...)}
	var a orderAlert
	if err := json.Unmarshal(env.Data, &a); err != nil {
		ev.Detail = "unparsed payload; see raw"
	} else {
		ev.Order = a.toOrder()
	}
	f.sink(ev)
}
