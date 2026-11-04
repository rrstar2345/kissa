// Package dhan implements the broker.Conn contract on top of DhanHQ v2:
// the REST order APIs plus the live order-update WebSocket.
package dhan

import (
	"context"
	"encoding/json"
	"net/http"
	"net/url"
	"time"

	"brokerproxy/internal/broker"
	"brokerproxy/internal/model"
)

const Name = "dhan"

var ist = time.FixedZone("IST", 5*3600+1800)

// Settings are the server-level Dhan options (not credentials).
type Settings struct {
	BaseURL         string
	FeedURL         string
	OrdersPerSecond float64
}

type conn struct {
	c      *client
	cancel context.CancelFunc
}

// NewOpener returns the broker.Opener for Dhan. Credentials required:
// "clientId" and "accessToken".
func NewOpener(s Settings) broker.Opener {
	return func(ctx context.Context, creds broker.Credentials, sink broker.Sink) (broker.Conn, error) {
		id, tok := creds["clientId"], creds["accessToken"]
		if id == "" || tok == "" {
			return nil, model.BadRequest("dhan credentials require clientId and accessToken")
		}
		cl := &client{
			http:     &http.Client{Timeout: 20 * time.Second},
			base:     s.BaseURL,
			clientID: id,
			token:    tok,
			lim:      newLimiter(s.OrdersPerSecond),
		}
		// Verify the token with a harmless read before accepting the session.
		vctx, cancel := context.WithTimeout(ctx, 15*time.Second)
		defer cancel()
		var probe json.RawMessage
		if err := cl.do(vctx, http.MethodGet, "/orders", nil, &probe, false); err != nil {
			return nil, err
		}
		// The feed outlives the opening request, so it gets its own context.
		fctx, stop := context.WithCancel(context.Background())
		f := &feed{url: s.FeedURL, clientID: id, token: tok, sink: sink}
		go f.run(fctx)
		return &conn{c: cl, cancel: stop}, nil
	}
}

func (d *conn) Name() string { return Name }

func (d *conn) Close() error {
	d.cancel() // stops the feed; the credentials are released with the conn
	return nil
}

func (d *conn) PlaceOrder(ctx context.Context, r *model.OrderRequest) (*model.OrderAck, error) {
	body := *r
	body.DhanClientID = d.c.clientID
	body.TradingSymbol = "" // not part of Dhan's request
	var ack model.OrderAck
	if err := d.c.do(ctx, http.MethodPost, "/orders", &body, &ack, true); err != nil {
		return nil, err
	}
	ack.Broker, ack.CorrelationID = Name, r.CorrelationID
	now := time.Now().In(ist).Format("2006-01-02 15:04:05")
	ack.Order = &model.Order{
		Broker: Name, DhanClientID: d.c.clientID, OrderID: ack.OrderID, CorrelationID: r.CorrelationID,
		OrderStatus: ack.OrderStatus, TransactionType: r.TransactionType, ExchangeSegment: r.ExchangeSegment,
		ProductType: r.ProductType, OrderType: r.OrderType, Validity: r.Validity,
		TradingSymbol: r.TradingSymbol, SecurityID: r.SecurityID, Quantity: r.Quantity,
		DisclosedQuantity: r.DisclosedQuantity, Price: r.Price, TriggerPrice: r.TriggerPrice,
		AfterMarketOrder: r.AfterMarketOrder, BoProfitValue: r.BoProfitValue, BoStopLossValue: r.BoStopLossValue,
		RemainingQuantity: r.Quantity, CreateTime: now, UpdateTime: now,
	}
	return &ack, nil
}

func (d *conn) ModifyOrder(ctx context.Context, id string, m *model.ModifyRequest) (*model.OrderAck, error) {
	body := *m
	body.DhanClientID, body.OrderID = d.c.clientID, id
	var ack model.OrderAck
	if err := d.c.do(ctx, http.MethodPut, "/orders/"+url.PathEscape(id), &body, &ack, true); err != nil {
		return nil, err
	}
	ack.Broker = Name
	return &ack, nil
}

func (d *conn) CancelOrder(ctx context.Context, id string) (*model.OrderAck, error) {
	var ack model.OrderAck
	if err := d.c.do(ctx, http.MethodDelete, "/orders/"+url.PathEscape(id), nil, &ack, true); err != nil {
		return nil, err
	}
	if ack.OrderID == "" {
		ack.OrderID = id
	}
	ack.Broker = Name
	return &ack, nil
}

// SliceOrder: Dhan documents no response shape for slicing, so both an array
// and a single object are accepted.
func (d *conn) SliceOrder(ctx context.Context, r *model.OrderRequest) ([]model.OrderAck, error) {
	body := *r
	body.DhanClientID = d.c.clientID
	body.TradingSymbol = ""
	var raw json.RawMessage
	if err := d.c.do(ctx, http.MethodPost, "/orders/slicing", &body, &raw, true); err != nil {
		return nil, err
	}
	var acks []model.OrderAck
	if err := json.Unmarshal(raw, &acks); err != nil {
		one, err2 := decodeOne[model.OrderAck](raw)
		if err2 != nil {
			return nil, model.Errf(http.StatusBadGateway, model.CodeUpstream, "unexpected slicing response: %v", err)
		}
		acks = []model.OrderAck{*one}
	}
	for i := range acks {
		acks[i].Broker, acks[i].CorrelationID = Name, r.CorrelationID
	}
	return acks, nil
}

func (d *conn) Orders(ctx context.Context) ([]model.Order, error) {
	var out []model.Order
	if err := d.c.do(ctx, http.MethodGet, "/orders", nil, &out, false); err != nil {
		return nil, err
	}
	for i := range out {
		out[i].Broker = Name
	}
	if out == nil {
		out = []model.Order{}
	}
	return out, nil
}

func (d *conn) getOrder(ctx context.Context, path string) (*model.Order, error) {
	var raw json.RawMessage
	if err := d.c.do(ctx, http.MethodGet, path, nil, &raw, false); err != nil {
		return nil, err
	}
	o, err := decodeOne[model.Order](raw)
	if err != nil {
		if ae, ok := err.(*model.APIError); ok {
			return nil, ae
		}
		return nil, model.Errf(http.StatusBadGateway, model.CodeUpstream, "unexpected dhan response: %v", err)
	}
	o.Broker = Name
	return o, nil
}

func (d *conn) Order(ctx context.Context, id string) (*model.Order, error) {
	return d.getOrder(ctx, "/orders/"+url.PathEscape(id))
}

func (d *conn) OrderByCorrelationID(ctx context.Context, corr string) (*model.Order, error) {
	return d.getOrder(ctx, "/orders/external/"+url.PathEscape(corr))
}

func (d *conn) trades(ctx context.Context, path string) ([]model.Trade, error) {
	var out []model.Trade
	if err := d.c.do(ctx, http.MethodGet, path, nil, &out, false); err != nil {
		return nil, err
	}
	for i := range out {
		out[i].Broker = Name
	}
	if out == nil {
		out = []model.Trade{}
	}
	return out, nil
}

func (d *conn) Trades(ctx context.Context) ([]model.Trade, error) {
	return d.trades(ctx, "/trades")
}

func (d *conn) TradesByOrder(ctx context.Context, id string) ([]model.Trade, error) {
	return d.trades(ctx, "/trades/"+url.PathEscape(id))
}
