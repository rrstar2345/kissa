package server_test

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gorilla/websocket"

	"brokerproxy/internal/auth"
	"brokerproxy/internal/broker"
	"brokerproxy/internal/broker/dhan"
	"brokerproxy/internal/broker/paper"
	"brokerproxy/internal/config"
	"brokerproxy/internal/server"
	"brokerproxy/internal/session"
)

// ---- helpers ----

type env struct {
	ts  *httptest.Server
	key string
}

func newEnv(t *testing.T, ds dhan.Settings) *env {
	t.Helper()
	key, hash := auth.GenerateKey()
	cfg := &config.Config{Clients: []config.Client{{ID: "c1", APIKeySHA256: hash}}}
	cfg.Session.IdleTimeoutMinutes, cfg.Session.MaxLifetimeHours = 60, 24
	a, err := auth.New(cfg.Clients)
	if err != nil {
		t.Fatal(err)
	}
	reg := broker.NewRegistry()
	reg.Register(paper.Name, paper.Open)
	reg.Register(dhan.Name, dhan.NewOpener(ds))
	mgr := session.NewManager(reg, session.Options{IdleTimeout: time.Hour, MaxLifetime: time.Hour, MaxPerClient: 5, ReplayBuffer: 3})
	ts := httptest.NewServer(server.New(cfg, reg, a, mgr).Handler())
	t.Cleanup(func() { mgr.Shutdown(); ts.Close() })
	return &env{ts: ts, key: key}
}

func (e *env) do(t *testing.T, method, path, sid string, body any) (int, map[string]any, http.Header) {
	t.Helper()
	var rd io.Reader
	if body != nil {
		b, _ := json.Marshal(body)
		rd = bytes.NewReader(b)
	}
	req, _ := http.NewRequest(method, e.ts.URL+path, rd)
	req.Header.Set("X-API-Key", e.key)
	if sid != "" {
		req.Header.Set("X-Session-Id", sid)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	var m map[string]any
	_ = json.Unmarshal(raw, &m)
	return resp.StatusCode, m, resp.Header
}

func (e *env) session(t *testing.T, brokerName string, creds map[string]string) string {
	t.Helper()
	code, m, _ := e.do(t, "POST", "/v1/sessions", "", map[string]any{"broker": brokerName, "credentials": creds})
	if code != 201 {
		t.Fatalf("create session: %d %v", code, m)
	}
	return m["sessionId"].(string)
}

func (e *env) dial(t *testing.T, sid, query string) *websocket.Conn {
	t.Helper()
	h := http.Header{"X-API-Key": {e.key}, "X-Session-Id": {sid}}
	c, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(e.ts.URL, "http")+"/v1/ws"+query, h)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

func readEvent(t *testing.T, c *websocket.Conn) map[string]any {
	t.Helper()
	_ = c.SetReadDeadline(time.Now().Add(3 * time.Second))
	var m map[string]any
	if err := c.ReadJSON(&m); err != nil {
		t.Fatalf("read event: %v", err)
	}
	return m
}

func limitOrder(corr string) map[string]any {
	return map[string]any{
		"transactionType": "BUY", "exchangeSegment": "NSE_EQ", "productType": "INTRADAY",
		"orderType": "LIMIT", "validity": "DAY", "securityId": "11536", "quantity": 5,
		"price": 3500.5, "correlationId": corr,
	}
}

// ---- paper broker ----

func TestPaperFlow(t *testing.T) {
	e := newEnv(t, dhan.Settings{})

	if code, _, _ := (&env{ts: e.ts, key: "bad"}).do(t, "POST", "/v1/sessions", "", map[string]any{"broker": "paper"}); code != 401 {
		t.Fatalf("bad key: want 401 got %d", code)
	}
	sid := e.session(t, "paper", nil)
	ws := e.dial(t, sid, "")
	if ev := readEvent(t, ws); ev["state"] != "subscribed" {
		t.Fatalf("hello: %v", ev)
	}

	// market orders are refused
	mo := limitOrder("m1")
	mo["orderType"] = "MARKET"
	if code, m, _ := e.do(t, "POST", "/v1/orders", sid, mo); code != 400 || m["error"].(map[string]any)["code"] != "market_order_not_allowed" {
		t.Fatalf("market order: %d %v", code, m)
	}
	// unknown field is refused
	bad := limitOrder("m2")
	bad["prise"] = 1
	if code, _, _ := e.do(t, "POST", "/v1/orders", sid, bad); code != 400 {
		t.Fatalf("typo field: want 400 got %d", code)
	}

	// LIMIT order fills at the limit price and returns an order id
	code, ack, _ := e.do(t, "POST", "/v1/orders", sid, limitOrder("o1"))
	if code != 200 || ack["orderStatus"] != "TRADED" || ack["orderId"] == "" {
		t.Fatalf("place: %d %v", code, ack)
	}
	o := ack["order"].(map[string]any)
	if o["averageTradedPrice"] != 3500.5 || o["filledQty"] != 5.0 {
		t.Fatalf("fill price/qty: %v", o)
	}
	// pushed over WSS: PENDING then TRADED, sequenced
	if ev := readEvent(t, ws); ev["type"] != "order_update" || ev["seq"] != 1.0 || ev["order"].(map[string]any)["orderStatus"] != "PENDING" {
		t.Fatalf("event 1: %v", ev)
	}
	if ev := readEvent(t, ws); ev["seq"] != 2.0 || ev["order"].(map[string]any)["orderStatus"] != "TRADED" {
		t.Fatalf("event 2: %v", ev)
	}

	// idempotent retry with the same correlationId does not place twice
	code, ack2, hdr := e.do(t, "POST", "/v1/orders", sid, limitOrder("o1"))
	if code != 200 || ack2["orderId"] != ack["orderId"] || hdr.Get("Idempotent-Replay") != "true" {
		t.Fatalf("replay: %d %v %v", code, ack2, hdr)
	}
	if _, orders, _ := e.do(t, "GET", "/v1/orders", sid, nil); orders != nil {
		t.Fatalf("orders list is an array, got object %v", orders)
	}

	// traded order can no longer be cancelled; lookups work
	if code, _, _ := e.do(t, "DELETE", "/v1/orders/"+ack["orderId"].(string), sid, nil); code != 400 {
		t.Fatalf("cancel traded: want 400 got %d", code)
	}
	if code, m, _ := e.do(t, "GET", "/v1/orders/external/o1", sid, nil); code != 200 || m["orderId"] != ack["orderId"] {
		t.Fatalf("by correlation: %d %v", code, m)
	}

	// STOP_LOSS stays pending and can be modified then cancelled
	sl := limitOrder("sl1")
	sl["orderType"], sl["triggerPrice"] = "STOP_LOSS", 3490.0
	_, slAck, _ := e.do(t, "POST", "/v1/orders", sid, sl)
	if slAck["orderStatus"] != "PENDING" {
		t.Fatalf("SL: %v", slAck)
	}
	id := slAck["orderId"].(string)
	mod := map[string]any{"orderType": "STOP_LOSS", "validity": "DAY", "price": 3480.0, "triggerPrice": 3485.0}
	if code, m, _ := e.do(t, "PUT", "/v1/orders/"+id, sid, mod); code != 200 || m["orderStatus"] != "PENDING" {
		t.Fatalf("modify: %d %v", code, m)
	}
	if code, m, _ := e.do(t, "DELETE", "/v1/orders/"+id, sid, nil); code != 202 || m["orderStatus"] != "CANCELLED" {
		t.Fatalf("cancel: %d %v", code, m)
	}

	// replay buffer is 3: resuming from 0 reports a gap, then the retained events
	ws2 := e.dial(t, sid, "?since=0")
	readEvent(t, ws2) // hello
	if ev := readEvent(t, ws2); ev["state"] != "gap" {
		t.Fatalf("expected gap notice, got %v", ev)
	}

	// another client's / unknown session is invisible; ending closes sockets
	if code, _, _ := e.do(t, "GET", "/v1/orders", "nope", nil); code != 404 {
		t.Fatalf("unknown session: %d", code)
	}
	if code, _, _ := e.do(t, "DELETE", "/v1/sessions/"+sid, "", nil); code != 204 {
		t.Fatalf("end session: %d", code)
	}
	_ = ws.SetReadDeadline(time.Now().Add(3 * time.Second))
	for {
		if _, _, err := ws.ReadMessage(); err != nil {
			if !websocket.IsCloseError(err, websocket.CloseGoingAway) {
				t.Fatalf("expected going-away close, got %v", err)
			}
			break
		}
	}
	if code, _, _ := e.do(t, "GET", "/v1/orders", sid, nil); code != 404 {
		t.Fatalf("ended session still usable: %d", code)
	}
}

func TestConcurrentSameCorrelationIDPlacesOnce(t *testing.T) {
	e := newEnv(t, dhan.Settings{})
	sid := e.session(t, "paper", nil)
	var wg sync.WaitGroup
	ids := make(chan string, 20)
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, m, _ := e.do(t, "POST", "/v1/orders", sid, limitOrder("race")); m != nil {
				ids <- m["orderId"].(string)
			}
		}()
	}
	wg.Wait()
	close(ids)
	seen := map[string]bool{}
	for id := range ids {
		seen[id] = true
	}
	if len(seen) != 1 {
		t.Fatalf("want exactly one order id, got %v", seen)
	}
}

// ---- dhan against a mock ----

func TestDhan(t *testing.T) {
	var mu sync.Mutex
	var placed map[string]any
	feedLogin := make(chan map[string]any, 1)
	up := websocket.Upgrader{}

	mock := http.NewServeMux()
	mock.HandleFunc("/v2/orders", func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("access-token") != "TOKEN" {
			http.Error(w, `{"errorType":"Invalid_Authentication","errorCode":"DH-901","errorMessage":"Client ID or user generated access token is invalid"}`, 401)
			return
		}
		if r.Method == "GET" {
			w.Write([]byte(`[]`))
			return
		}
		mu.Lock()
		_ = json.NewDecoder(r.Body).Decode(&placed)
		mu.Unlock()
		w.Write([]byte(`{"orderId":"112111182198","orderStatus":"PENDING"}`))
	})
	mock.HandleFunc("/feed", func(w http.ResponseWriter, r *http.Request) {
		c, err := up.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer c.Close()
		var login map[string]any
		_ = c.ReadJSON(&login)
		feedLogin <- login
		_ = c.WriteMessage(websocket.TextMessage, []byte(`{"Type":"order_alert","Data":{"Exchange":"NSE","Segment":"E","SecurityId":"11536","ClientId":"1000000001","OrderNo":"112111182198","ExchOrderNo":"X1","Product":"I","TxnType":"B","OrderType":"LMT","Validity":"DAY","Quantity":5,"TradedQty":5,"RemainingQuantity":0,"Price":3500.5,"AvgTradedPrice":3500.5,"Status":"TRADED","Symbol":"TCS","CorrelationId":"d1","LegNo":1}}`))
		_, _, _ = c.ReadMessage() // hold the socket open until the proxy closes it
	})
	ms := httptest.NewServer(mock)
	defer ms.Close()

	e := newEnv(t, dhan.Settings{
		BaseURL: ms.URL + "/v2", FeedURL: "ws" + strings.TrimPrefix(ms.URL, "http") + "/feed", OrdersPerSecond: 100,
	})

	// bad token is rejected at session creation
	code, m, _ := e.do(t, "POST", "/v1/sessions", "", map[string]any{"broker": "dhan", "credentials": map[string]string{"clientId": "1000000001", "accessToken": "WRONG"}})
	if code != 424 || m["error"].(map[string]any)["code"] != "broker_unauthorized" {
		t.Fatalf("bad token: %d %v", code, m)
	}

	sid := e.session(t, "dhan", map[string]string{"clientId": "1000000001", "accessToken": "TOKEN"})
	ws := e.dial(t, sid, "")
	readEvent(t, ws) // hello

	if login := <-feedLogin; login["UserType"] != "SELF" || login["LoginReq"].(map[string]any)["Token"] != "TOKEN" {
		t.Fatalf("feed login: %v", login)
	}

	// feed: connected status, then the normalized order update
	var upd map[string]any
	for i := 0; i < 3 && upd == nil; i++ {
		if ev := readEvent(t, ws); ev["type"] == "order_update" {
			upd = ev
		}
	}
	if upd == nil {
		t.Fatal("no order_update received")
	}
	o := upd["order"].(map[string]any)
	if o["orderId"] != "112111182198" || o["transactionType"] != "BUY" || o["productType"] != "INTRADAY" ||
		o["orderType"] != "LIMIT" || o["exchangeSegment"] != "NSE_EQ" || o["orderStatus"] != "TRADED" || upd["raw"] == nil {
		t.Fatalf("normalized update: %v", upd)
	}

	// place: Dhan receives the session's client id, not the caller's
	req := limitOrder("d1")
	req["dhanClientId"] = "SOMEONE-ELSE"
	code, ack, _ := e.do(t, "POST", "/v1/orders", sid, req)
	if code != 200 || ack["orderId"] != "112111182198" || ack["broker"] != "dhan" {
		t.Fatalf("dhan place: %d %v", code, ack)
	}
	mu.Lock()
	defer mu.Unlock()
	if placed["dhanClientId"] != "1000000001" || placed["orderType"] != "LIMIT" || placed["price"] != 3500.5 {
		t.Fatalf("payload sent to dhan: %v", placed)
	}
}
