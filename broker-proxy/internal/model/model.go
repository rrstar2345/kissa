// Package model holds the broker-neutral wire types. JSON field names mirror
// the DhanHQ v2 Orders API so other brokers can be mapped onto the same shape.
package model

import "encoding/json"

// OrderRequest mirrors Dhan POST /orders (and /orders/slicing).
type OrderRequest struct {
	DhanClientID      string  `json:"dhanClientId,omitempty"` // ignored; set from session credentials
	CorrelationID     string  `json:"correlationId,omitempty"`
	TransactionType   string  `json:"transactionType"`
	ExchangeSegment   string  `json:"exchangeSegment"`
	ProductType       string  `json:"productType"`
	OrderType         string  `json:"orderType"`
	Validity          string  `json:"validity"`
	SecurityID        string  `json:"securityId"`
	TradingSymbol     string  `json:"tradingSymbol,omitempty"` // optional; echoed back, not sent to Dhan
	Quantity          int     `json:"quantity"`
	DisclosedQuantity int     `json:"disclosedQuantity,omitempty"`
	Price             float64 `json:"price"`
	TriggerPrice      float64 `json:"triggerPrice,omitempty"`
	AfterMarketOrder  bool    `json:"afterMarketOrder,omitempty"`
	AmoTime           string  `json:"amoTime,omitempty"`
	BoProfitValue     float64 `json:"boProfitValue,omitempty"`
	BoStopLossValue   float64 `json:"boStopLossValue,omitempty"`
}

// ModifyRequest mirrors Dhan PUT /orders/{order-id}.
type ModifyRequest struct {
	DhanClientID      string  `json:"dhanClientId,omitempty"`
	OrderID           string  `json:"orderId,omitempty"` // set from the URL path
	OrderType         string  `json:"orderType"`
	LegName           string  `json:"legName,omitempty"`
	Quantity          int     `json:"quantity,omitempty"`
	Price             float64 `json:"price,omitempty"`
	DisclosedQuantity int     `json:"disclosedQuantity,omitempty"`
	TriggerPrice      float64 `json:"triggerPrice,omitempty"`
	Validity          string  `json:"validity"`
}

// OrderAck is returned by place / modify / cancel / slice. Broker and Order
// are proxy additions so the client can persist everything from one response.
type OrderAck struct {
	OrderID       string `json:"orderId"`
	OrderStatus   string `json:"orderStatus"`
	CorrelationID string `json:"correlationId,omitempty"`
	Broker        string `json:"broker"`
	Order         *Order `json:"order,omitempty"` // best-known snapshot (place only for Dhan)
}

// Order mirrors a Dhan order book entry.
type Order struct {
	Broker              string  `json:"broker,omitempty"`
	DhanClientID        string  `json:"dhanClientId,omitempty"`
	OrderID             string  `json:"orderId"`
	CorrelationID       string  `json:"correlationId,omitempty"`
	OrderStatus         string  `json:"orderStatus"`
	TransactionType     string  `json:"transactionType"`
	ExchangeSegment     string  `json:"exchangeSegment"`
	ProductType         string  `json:"productType"`
	OrderType           string  `json:"orderType"`
	Validity            string  `json:"validity"`
	TradingSymbol       string  `json:"tradingSymbol,omitempty"`
	SecurityID          string  `json:"securityId"`
	Quantity            int     `json:"quantity"`
	DisclosedQuantity   int     `json:"disclosedQuantity"`
	Price               float64 `json:"price"`
	TriggerPrice        float64 `json:"triggerPrice"`
	AfterMarketOrder    bool    `json:"afterMarketOrder"`
	BoProfitValue       float64 `json:"boProfitValue"`
	BoStopLossValue     float64 `json:"boStopLossValue"`
	LegName             string  `json:"legName,omitempty"`
	CreateTime          string  `json:"createTime,omitempty"`
	UpdateTime          string  `json:"updateTime,omitempty"`
	ExchangeTime        string  `json:"exchangeTime,omitempty"`
	DrvExpiryDate       string  `json:"drvExpiryDate,omitempty"`
	DrvOptionType       string  `json:"drvOptionType,omitempty"`
	DrvStrikePrice      float64 `json:"drvStrikePrice"`
	OmsErrorCode        string  `json:"omsErrorCode,omitempty"`
	OmsErrorDescription string  `json:"omsErrorDescription,omitempty"`
	AlgoID              string  `json:"algoId,omitempty"`
	RemainingQuantity   int     `json:"remainingQuantity"`
	AverageTradedPrice  float64 `json:"averageTradedPrice"`
	FilledQty           int     `json:"filledQty"`
}

// Trade mirrors a Dhan trade book entry.
type Trade struct {
	Broker          string  `json:"broker,omitempty"`
	DhanClientID    string  `json:"dhanClientId,omitempty"`
	OrderID         string  `json:"orderId"`
	ExchangeOrderID string  `json:"exchangeOrderId"`
	ExchangeTradeID string  `json:"exchangeTradeId"`
	TransactionType string  `json:"transactionType"`
	ExchangeSegment string  `json:"exchangeSegment"`
	ProductType     string  `json:"productType"`
	OrderType       string  `json:"orderType"`
	TradingSymbol   string  `json:"tradingSymbol,omitempty"`
	SecurityID      string  `json:"securityId"`
	TradedQuantity  int     `json:"tradedQuantity"`
	TradedPrice     float64 `json:"tradedPrice"`
	CreateTime      string  `json:"createTime,omitempty"`
	UpdateTime      string  `json:"updateTime,omitempty"`
	ExchangeTime    string  `json:"exchangeTime,omitempty"`
	DrvExpiryDate   string  `json:"drvExpiryDate,omitempty"`
	DrvOptionType   string  `json:"drvOptionType,omitempty"`
	DrvStrikePrice  float64 `json:"drvStrikePrice"`
}

// Event types pushed to clients over the WSS.
const (
	EventOrderUpdate = "order_update"
	EventFeedStatus  = "feed_status"
	EventSession     = "session" // e.g. session ended
)

// Event is the single envelope sent on the client WSS. Seq and Time are
// assigned by the session hub; Seq is monotonically increasing per session
// so a client can detect gaps and resume with ?since=<lastSeq>.
type Event struct {
	Type   string          `json:"type"`
	Seq    uint64          `json:"seq"`
	Time   string          `json:"time"`
	Broker string          `json:"broker"`
	Order  *Order          `json:"order,omitempty"`  // order_update: normalized snapshot
	Raw    json.RawMessage `json:"raw,omitempty"`    // order_update: untouched broker payload
	State  string          `json:"state,omitempty"`  // feed_status: connected | disconnected
	Detail string          `json:"detail,omitempty"` // free text
}
