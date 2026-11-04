package server

import (
	"net/http"
	"strconv"
	"time"

	"github.com/gorilla/websocket"

	"brokerproxy/internal/model"
	"brokerproxy/internal/session"
)

const (
	wsWriteWait = 10 * time.Second
	wsPongWait  = 60 * time.Second
	wsPingEvery = 25 * time.Second
	wsBuffer    = 256
)

// Origin checks protect browsers; this API authenticates with a custom header
// that browsers cannot attach to a WebSocket, so any origin may connect.
var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 4096,
	CheckOrigin:     func(*http.Request) bool { return true },
}

// ws streams the session's live events to one client connection:
//
//	GET /v1/ws[?since=<seq>]   headers: X-API-Key, X-Session-Id
//
// Without since, only new events are sent. With since=N, buffered events with
// seq > N are replayed first, then live events follow without gaps.
func (s *Server) ws(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	since := int64(-1)
	if v := r.URL.Query().Get("since"); v != "" {
		n, err := strconv.ParseInt(v, 10, 64)
		if err != nil || n < 0 {
			writeErr(w, model.BadRequest("since must be a non-negative integer"))
			return
		}
		since = n
	}
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return // Upgrade already wrote the HTTP error
	}
	defer conn.Close()

	sub, backlog := sess.Hub.Subscribe(since, wsBuffer)
	defer sess.Hub.Unsubscribe(sub)

	// Reader: handles pongs and detects client disconnects. Client messages are ignored.
	done := make(chan struct{})
	conn.SetReadLimit(4096)
	_ = conn.SetReadDeadline(time.Now().Add(wsPongWait))
	conn.SetPongHandler(func(string) error { return conn.SetReadDeadline(time.Now().Add(wsPongWait)) })
	go func() {
		defer close(done)
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				return
			}
			_ = conn.SetReadDeadline(time.Now().Add(wsPongWait))
		}
	}()

	send := func(e model.Event) bool {
		_ = conn.SetWriteDeadline(time.Now().Add(wsWriteWait))
		return conn.WriteJSON(e) == nil
	}

	if !send(model.Event{Type: model.EventSession, Broker: sess.Broker, State: "subscribed",
		Time: time.Now().Format(time.RFC3339)}) {
		return
	}
	for _, e := range backlog {
		if !send(e) {
			return
		}
	}

	ping := time.NewTicker(wsPingEvery)
	defer ping.Stop()
	for {
		select {
		case e, ok := <-sub.C:
			if !ok {
				reason := sub.Reason
				if len(reason) > 120 {
					reason = reason[:120]
				}
				_ = conn.WriteControl(websocket.CloseMessage,
					websocket.FormatCloseMessage(websocket.CloseGoingAway, reason), time.Now().Add(wsWriteWait))
				return
			}
			if !send(e) {
				return
			}
		case <-ping.C:
			if conn.WriteControl(websocket.PingMessage, nil, time.Now().Add(wsWriteWait)) != nil {
				return
			}
		case <-done:
			return
		}
	}
}
