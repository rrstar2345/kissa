package server

import (
	"context"
	"net/http"
	"regexp"
	"time"

	"brokerproxy/internal/broker"
	"brokerproxy/internal/model"
	"brokerproxy/internal/session"
)

var (
	orderIDRe = regexp.MustCompile(`^[A-Za-z0-9_-]{1,64}$`)
	corrIDRe  = regexp.MustCompile(`^[a-zA-Z0-9 _-]{1,30}$`)
)

const writeTimeout = 25 * time.Second

// writeCtx detaches order-changing calls from the client's connection so an
// order that has been sent to the broker is never abandoned half-way when the
// client disconnects; the idempotency layer then records the real outcome.
func writeCtx(r *http.Request) (context.Context, context.CancelFunc) {
	return context.WithTimeout(context.WithoutCancel(r.Context()), writeTimeout)
}

// ---- sessions ----

type createSessionReq struct {
	Broker      string            `json:"broker"`
	Credentials map[string]string `json:"credentials"`
}

func (s *Server) createSession(w http.ResponseWriter, r *http.Request, clientID string) {
	var req createSessionReq
	if err := readJSON(w, r, &req); err != nil {
		writeErr(w, err)
		return
	}
	if req.Broker == "" || len(req.Credentials) > 10 {
		writeErr(w, model.BadRequest("broker is required and credentials must have at most 10 entries"))
		return
	}
	sess, err := s.mgr.Create(r.Context(), clientID, req.Broker, broker.Credentials(req.Credentials))
	if err != nil {
		writeErr(w, err)
		return
	}
	writeJSON(w, http.StatusCreated, map[string]any{
		"sessionId":          sess.ID,
		"broker":             sess.Broker,
		"createdAt":          sess.CreatedAt.Format(time.RFC3339),
		"idleTimeoutMinutes": s.cfg.Session.IdleTimeoutMinutes,
		"maxLifetimeHours":   s.cfg.Session.MaxLifetimeHours,
		"websocket":          "/v1/ws",
	})
}

func (s *Server) endSession(w http.ResponseWriter, r *http.Request, clientID string) {
	if err := s.mgr.End(r.PathValue("id"), clientID, "ended by client"); err != nil {
		writeErr(w, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

// ---- orders ----

func (s *Server) placeOrder(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	var req model.OrderRequest
	if err := readJSON(w, r, &req); err != nil {
		writeErr(w, err)
		return
	}
	if err := req.Validate(); err != nil {
		writeErr(w, err)
		return
	}
	ctx, cancel := writeCtx(r)
	defer cancel()
	ack, replay, err := sess.PlaceOnce(ctx, &req)
	if err != nil {
		writeErr(w, err)
		return
	}
	if replay {
		w.Header().Set("Idempotent-Replay", "true")
	}
	writeJSON(w, http.StatusOK, ack)
}

func (s *Server) sliceOrder(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	var req model.OrderRequest
	if err := readJSON(w, r, &req); err != nil {
		writeErr(w, err)
		return
	}
	if err := req.Validate(); err != nil {
		writeErr(w, err)
		return
	}
	ctx, cancel := writeCtx(r)
	defer cancel()
	acks, err := sess.Conn.SliceOrder(ctx, &req)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeJSON(w, http.StatusOK, acks)
}

func orderID(w http.ResponseWriter, r *http.Request) (string, bool) {
	id := r.PathValue("orderId")
	if !orderIDRe.MatchString(id) {
		writeErr(w, model.BadRequest("invalid orderId"))
		return "", false
	}
	return id, true
}

func (s *Server) modifyOrder(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	id, ok := orderID(w, r)
	if !ok {
		return
	}
	var req model.ModifyRequest
	if err := readJSON(w, r, &req); err != nil {
		writeErr(w, err)
		return
	}
	if req.OrderID != "" && req.OrderID != id {
		writeErr(w, model.BadRequest("orderId in body does not match the URL"))
		return
	}
	if err := req.Validate(); err != nil {
		writeErr(w, err)
		return
	}
	ctx, cancel := writeCtx(r)
	defer cancel()
	ack, err := sess.Conn.ModifyOrder(ctx, id, &req)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeJSON(w, http.StatusOK, ack)
}

func (s *Server) cancelOrder(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	id, ok := orderID(w, r)
	if !ok {
		return
	}
	ctx, cancel := writeCtx(r)
	defer cancel()
	ack, err := sess.Conn.CancelOrder(ctx, id)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeJSON(w, http.StatusAccepted, ack)
}

func (s *Server) listOrders(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	out, err := sess.Conn.Orders(r.Context())
	reply(w, out, err)
}

func (s *Server) getOrder(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	id, ok := orderID(w, r)
	if !ok {
		return
	}
	out, err := sess.Conn.Order(r.Context(), id)
	reply(w, out, err)
}

func (s *Server) orderByCorrelation(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	id := r.PathValue("correlationId")
	if !corrIDRe.MatchString(id) {
		writeErr(w, model.BadRequest("invalid correlationId"))
		return
	}
	out, err := sess.Conn.OrderByCorrelationID(r.Context(), id)
	reply(w, out, err)
}

func (s *Server) listTrades(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	out, err := sess.Conn.Trades(r.Context())
	reply(w, out, err)
}

func (s *Server) tradesByOrder(w http.ResponseWriter, r *http.Request, sess *session.Session) {
	id, ok := orderID(w, r)
	if !ok {
		return
	}
	out, err := sess.Conn.TradesByOrder(r.Context(), id)
	reply(w, out, err)
}

func reply(w http.ResponseWriter, v any, err error) {
	if err != nil {
		writeErr(w, err)
		return
	}
	writeJSON(w, http.StatusOK, v)
}
