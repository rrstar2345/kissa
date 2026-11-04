// Package paper is a simulated broker with the same surface as Dhan. State is
// per session and in memory only. Orders are filled by an Executor; the
// default executor fills LIMIT orders immediately at their limit price and
// leaves STOP_LOSS orders pending (there is no market data yet to trigger
// them). Replace the Executor later to wire in real paper-trading logic.
package paper

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"net/http"
	"sync"
	"time"

	"brokerproxy/internal/broker"
	"brokerproxy/internal/model"
)

const Name = "paper"

var ist = time.FixedZone("IST", 5*3600+1800)

func stamp() string { return time.Now().In(ist).Format("2006-01-02 15:04:05") }

// Executor decides whether a pending order fills right now and at what price.
type Executor func(o *model.Order) (fill bool, price float64)

// LimitFill is the default executor: LIMIT orders execute at the limit price.
func LimitFill(o *model.Order) (bool, float64) {
	if o.OrderType == "LIMIT" {
		return true, o.Price
	}
	return false, 0
}

type conn struct {
	exec Executor
	sink broker.Sink

	mu     sync.Mutex
	prefix string
	n      int
	orders map[string]*model.Order
	ids    []string
	corr   map[string]string
	trades []model.Trade
	closed bool
}

// Open is the broker.Opener for the paper broker. No credentials are needed.
func Open(_ context.Context, _ broker.Credentials, sink broker.Sink) (broker.Conn, error) {
	return New(sink, LimitFill), nil
}

// New builds a paper connection with a custom executor.
func New(sink broker.Sink, exec Executor) broker.Conn {
	var b [2]byte
	_, _ = rand.Read(b[:])
	return &conn{
		exec:   exec,
		sink:   sink,
		prefix: "PT" + time.Now().In(ist).Format("060102") + hex.EncodeToString(b[:]),
		orders: map[string]*model.Order{},
		corr:   map[string]string{},
	}
}

func (c *conn) Name() string { return Name }

func (c *conn) Close() error {
	c.mu.Lock()
	c.closed = true
	c.mu.Unlock()
	return nil
}

// event snapshots an order into an order_update event. Caller holds c.mu.
func (c *conn) event(o *model.Order) model.Event {
	cp := *o
	return model.Event{Type: model.EventOrderUpdate, Broker: Name, Order: &cp}
}

func (c *conn) emit(evs []model.Event) {
	if c.sink == nil {
		return
	}
	for _, e := range evs {
		c.sink(e)
	}
}

// fill marks o fully traded at px and records the trade. Caller holds c.mu.
func (c *conn) fill(o *model.Order, px float64) {
	now := stamp()
	o.OrderStatus = "TRADED"
	o.FilledQty = o.Quantity
	o.RemainingQuantity = 0
	o.AverageTradedPrice = px
	o.ExchangeTime = now
	o.UpdateTime = now
	c.trades = append(c.trades, model.Trade{
		Broker: Name, DhanClientID: o.DhanClientID, OrderID: o.OrderID,
		ExchangeOrderID: "X" + o.OrderID, ExchangeTradeID: fmt.Sprintf("T%s-%d", o.OrderID, len(c.trades)+1),
		TransactionType: o.TransactionType, ExchangeSegment: o.ExchangeSegment,
		ProductType: o.ProductType, OrderType: o.OrderType, TradingSymbol: o.TradingSymbol,
		SecurityID: o.SecurityID, TradedQuantity: o.Quantity, TradedPrice: px,
		CreateTime: now, UpdateTime: now, ExchangeTime: now,
	})
}

func (c *conn) PlaceOrder(_ context.Context, r *model.OrderRequest) (*model.OrderAck, error) {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return nil, model.Errf(http.StatusGone, model.CodeInternal, "paper connection closed")
	}
	c.n++
	id := fmt.Sprintf("%s%05d", c.prefix, c.n)
	now := stamp()
	o := &model.Order{
		Broker: Name, DhanClientID: "paper", OrderID: id, CorrelationID: r.CorrelationID,
		OrderStatus: "PENDING", TransactionType: r.TransactionType, ExchangeSegment: r.ExchangeSegment,
		ProductType: r.ProductType, OrderType: r.OrderType, Validity: r.Validity,
		TradingSymbol: r.TradingSymbol, SecurityID: r.SecurityID, Quantity: r.Quantity,
		DisclosedQuantity: r.DisclosedQuantity, Price: r.Price, TriggerPrice: r.TriggerPrice,
		AfterMarketOrder: r.AfterMarketOrder, BoProfitValue: r.BoProfitValue, BoStopLossValue: r.BoStopLossValue,
		RemainingQuantity: r.Quantity, CreateTime: now, UpdateTime: now,
	}
	c.orders[id] = o
	c.ids = append(c.ids, id)
	if r.CorrelationID != "" {
		c.corr[r.CorrelationID] = id
	}
	evs := []model.Event{c.event(o)}
	if ok, px := c.exec(o); ok {
		c.fill(o, px)
		evs = append(evs, c.event(o))
	}
	snap := *o
	c.mu.Unlock()

	c.emit(evs)
	return &model.OrderAck{OrderID: id, OrderStatus: snap.OrderStatus, CorrelationID: snap.CorrelationID, Broker: Name, Order: &snap}, nil
}

func (c *conn) ModifyOrder(_ context.Context, id string, m *model.ModifyRequest) (*model.OrderAck, error) {
	c.mu.Lock()
	o, ok := c.orders[id]
	if !ok {
		c.mu.Unlock()
		return nil, model.NotFound("order %s not found", id)
	}
	if o.OrderStatus != "PENDING" {
		st := o.OrderStatus
		c.mu.Unlock()
		return nil, model.Errf(http.StatusBadRequest, model.CodeNotModifiable, "order %s is %s and cannot be modified", id, st)
	}
	o.OrderType, o.Validity, o.Price = m.OrderType, m.Validity, m.Price
	if m.Quantity > 0 {
		o.Quantity, o.RemainingQuantity = m.Quantity, m.Quantity
	}
	if m.DisclosedQuantity > 0 {
		o.DisclosedQuantity = m.DisclosedQuantity
	}
	if m.TriggerPrice > 0 {
		o.TriggerPrice = m.TriggerPrice
	}
	o.UpdateTime = stamp()
	evs := []model.Event{c.event(o)}
	if ok, px := c.exec(o); ok {
		c.fill(o, px)
		evs = append(evs, c.event(o))
	}
	status := o.OrderStatus
	c.mu.Unlock()

	c.emit(evs)
	return &model.OrderAck{OrderID: id, OrderStatus: status, Broker: Name}, nil
}

func (c *conn) CancelOrder(_ context.Context, id string) (*model.OrderAck, error) {
	c.mu.Lock()
	o, ok := c.orders[id]
	if !ok {
		c.mu.Unlock()
		return nil, model.NotFound("order %s not found", id)
	}
	if o.OrderStatus != "PENDING" {
		st := o.OrderStatus
		c.mu.Unlock()
		return nil, model.Errf(http.StatusBadRequest, model.CodeNotModifiable, "order %s is %s and cannot be cancelled", id, st)
	}
	o.OrderStatus = "CANCELLED"
	o.UpdateTime = stamp()
	ev := c.event(o)
	c.mu.Unlock()

	c.emit([]model.Event{ev})
	return &model.OrderAck{OrderID: id, OrderStatus: "CANCELLED", Broker: Name}, nil
}

// SliceOrder: the paper broker has no freeze limit, so the order is placed whole.
func (c *conn) SliceOrder(ctx context.Context, r *model.OrderRequest) ([]model.OrderAck, error) {
	a, err := c.PlaceOrder(ctx, r)
	if err != nil {
		return nil, err
	}
	return []model.OrderAck{*a}, nil
}

func (c *conn) Orders(context.Context) ([]model.Order, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	out := make([]model.Order, 0, len(c.ids))
	for _, id := range c.ids {
		out = append(out, *c.orders[id])
	}
	return out, nil
}

func (c *conn) Order(_ context.Context, id string) (*model.Order, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	o, ok := c.orders[id]
	if !ok {
		return nil, model.NotFound("order %s not found", id)
	}
	cp := *o
	return &cp, nil
}

func (c *conn) OrderByCorrelationID(ctx context.Context, corr string) (*model.Order, error) {
	c.mu.Lock()
	id, ok := c.corr[corr]
	c.mu.Unlock()
	if !ok {
		return nil, model.NotFound("no order with correlationId %q", corr)
	}
	return c.Order(ctx, id)
}

func (c *conn) Trades(context.Context) ([]model.Trade, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return append([]model.Trade(nil), c.trades...), nil
}

func (c *conn) TradesByOrder(_ context.Context, id string) ([]model.Trade, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if _, ok := c.orders[id]; !ok {
		return nil, model.NotFound("order %s not found", id)
	}
	out := []model.Trade{}
	for _, t := range c.trades {
		if t.OrderID == id {
			out = append(out, t)
		}
	}
	return out, nil
}
