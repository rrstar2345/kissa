package session

import (
	"sync"
	"time"

	"brokerproxy/internal/model"
)

var ist = time.FixedZone("IST", 5*3600+1800)

// Subscriber is one client WebSocket attached to a session's event stream.
// C is closed when the subscriber is dropped (slow consumer, unsubscribe) or
// the session ends; Reason then says why.
type Subscriber struct {
	C      chan model.Event
	Reason string
}

// Hub fans broker events out to subscribers, stamps each with a per-session
// sequence number and keeps a bounded replay buffer so a reconnecting client
// can resume with ?since=<lastSeq>. Publishing never blocks: a subscriber that
// cannot keep up is dropped and must reconnect.
type Hub struct {
	broker string
	size   int

	mu     sync.Mutex
	seq    uint64
	ring   []model.Event
	subs   map[*Subscriber]struct{}
	closed bool
}

func NewHub(broker string, replay int) *Hub {
	return &Hub{broker: broker, size: replay, subs: map[*Subscriber]struct{}{}}
}

// Publish is the broker.Sink for the session.
func (h *Hub) Publish(e model.Event) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed {
		return
	}
	h.seq++
	e.Seq = h.seq
	e.Time = time.Now().In(ist).Format("2006-01-02T15:04:05.000-07:00")
	if e.Broker == "" {
		e.Broker = h.broker
	}
	h.ring = append(h.ring, e)
	if len(h.ring) > h.size {
		h.ring = append([]model.Event(nil), h.ring[len(h.ring)-h.size:]...)
	}
	for s := range h.subs {
		select {
		case s.C <- e:
		default:
			s.Reason = "slow consumer: reconnect with ?since=<lastSeq>"
			close(s.C)
			delete(h.subs, s)
		}
	}
}

// Subscribe attaches a subscriber. since < 0 means live events only;
// otherwise the buffered events with Seq > since are returned as backlog
// (preceded by a notice if some were already evicted). Registration and
// backlog capture are atomic, so no event is missed or duplicated.
func (h *Hub) Subscribe(since int64, buf int) (*Subscriber, []model.Event) {
	h.mu.Lock()
	defer h.mu.Unlock()
	s := &Subscriber{C: make(chan model.Event, buf)}
	if h.closed {
		s.Reason = "session ended"
		close(s.C)
		return s, nil
	}
	var backlog []model.Event
	if since >= 0 {
		oldest := h.seq - uint64(len(h.ring)) + 1
		if h.seq > uint64(since) && uint64(since)+1 < oldest {
			backlog = append(backlog, model.Event{
				Type: model.EventSession, Broker: h.broker, Seq: uint64(since),
				Time:   time.Now().In(ist).Format("2006-01-02T15:04:05.000-07:00"),
				State:  "gap",
				Detail: "events before the replay buffer were dropped; resync with GET /v1/orders",
			})
		}
		for _, e := range h.ring {
			if e.Seq > uint64(since) {
				backlog = append(backlog, e)
			}
		}
	}
	h.subs[s] = struct{}{}
	return s, backlog
}

func (h *Hub) Unsubscribe(s *Subscriber) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if _, ok := h.subs[s]; ok {
		delete(h.subs, s)
		close(s.C)
	}
}

func (h *Hub) Subscribers() int {
	h.mu.Lock()
	defer h.mu.Unlock()
	return len(h.subs)
}

// Close tells every subscriber the session ended and detaches them.
func (h *Hub) Close(reason string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.closed {
		return
	}
	h.closed = true
	for s := range h.subs {
		s.Reason = reason
		close(s.C)
		delete(h.subs, s)
	}
	h.ring = nil
}
