package dhan

import (
	"strconv"
	"strings"

	"brokerproxy/internal/model"
)

// Dhan's feed documentation is loose about number/string typing, so these
// types accept either form and never fail the whole message on one field.

type flexStr string

func (s *flexStr) UnmarshalJSON(b []byte) error {
	t := strings.TrimSpace(string(b))
	if t == "null" {
		*s = ""
		return nil
	}
	*s = flexStr(strings.Trim(t, `"`))
	return nil
}

type flexFloat float64

func (f *flexFloat) UnmarshalJSON(b []byte) error {
	t := strings.Trim(strings.TrimSpace(string(b)), `"`)
	v, err := strconv.ParseFloat(t, 64)
	if err != nil {
		v = 0
	}
	*f = flexFloat(v)
	return nil
}

type flexInt int

func (i *flexInt) UnmarshalJSON(b []byte) error {
	var f flexFloat
	_ = f.UnmarshalJSON(b)
	*i = flexInt(f)
	return nil
}

// orderAlert is the "Data" object of an order_alert message on the live feed.
// encoding/json matches keys case-insensitively, which also covers the few
// lower-case keys in Dhan's sample.
type orderAlert struct {
	Exchange          flexStr
	Segment           flexStr
	SecurityId        flexStr
	ClientId          flexStr
	ExchOrderNo       flexStr
	OrderNo           flexStr
	Product           flexStr
	TxnType           flexStr
	OrderType         flexStr
	Validity          flexStr
	DiscQuantity      flexInt
	RemainingQuantity flexInt
	Quantity          flexInt
	TradedQty         flexInt
	Price             flexFloat
	TriggerPrice      flexFloat
	TradedPrice       flexFloat
	AvgTradedPrice    flexFloat
	OffMktFlag        flexStr
	OrderDateTime     flexStr
	ExchOrderTime     flexStr
	LastUpdatedTime   flexStr
	ReasonDescription flexStr
	LegNo             flexInt
	Symbol            flexStr
	DisplayName       flexStr
	Status            flexStr
	StrikePrice       flexFloat
	ExpiryDate        flexStr
	OptType           flexStr
	AlgoId            flexStr
	CorrelationId     flexStr
}

var (
	productNames   = map[string]string{"C": "CNC", "I": "INTRADAY", "M": "MARGIN", "F": "MTF", "V": "CO", "B": "BO"}
	txnNames       = map[string]string{"B": "BUY", "S": "SELL"}
	orderTypeNames = map[string]string{"LMT": "LIMIT", "MKT": "MARKET", "SL": "STOP_LOSS", "SLM": "STOP_LOSS_MARKET"}
	legNames       = map[int]string{1: "ENTRY_LEG", 2: "STOP_LOSS_LEG", 3: "TARGET_LEG"}
	segmentNames   = map[string]string{"E": "EQ", "D": "FNO", "C": "CURRENCY", "M": "COMM"}
)

func lookup(m map[string]string, k string) string {
	if v, ok := m[k]; ok {
		return v
	}
	return k // unknown codes pass through unchanged
}

func exchangeSegment(exchange, segment string) string {
	seg, ok := segmentNames[segment]
	if !ok || exchange == "" {
		return ""
	}
	return exchange + "_" + seg
}

// toOrder converts a live-feed payload into the REST order shape.
func (a *orderAlert) toOrder() *model.Order {
	status := string(a.Status)
	if status == "PENDING" && a.TradedQty > 0 && a.TradedQty < a.Quantity {
		status = "PART_TRADED"
	}
	opt := ""
	switch string(a.OptType) {
	case "CE":
		opt = "CALL"
	case "PE":
		opt = "PUT"
	}
	sym := string(a.Symbol)
	if sym == "" {
		sym = string(a.DisplayName)
	}
	return &model.Order{
		Broker:              Name,
		DhanClientID:        string(a.ClientId),
		OrderID:             string(a.OrderNo),
		CorrelationID:       string(a.CorrelationId),
		OrderStatus:         status,
		TransactionType:     lookup(txnNames, string(a.TxnType)),
		ExchangeSegment:     exchangeSegment(string(a.Exchange), string(a.Segment)),
		ProductType:         lookup(productNames, string(a.Product)),
		OrderType:           lookup(orderTypeNames, string(a.OrderType)),
		Validity:            string(a.Validity),
		TradingSymbol:       sym,
		SecurityID:          string(a.SecurityId),
		Quantity:            int(a.Quantity),
		DisclosedQuantity:   int(a.DiscQuantity),
		Price:               float64(a.Price),
		TriggerPrice:        float64(a.TriggerPrice),
		AfterMarketOrder:    string(a.OffMktFlag) == "1",
		LegName:             legNames[int(a.LegNo)],
		CreateTime:          string(a.OrderDateTime),
		UpdateTime:          string(a.LastUpdatedTime),
		ExchangeTime:        string(a.ExchOrderTime),
		DrvExpiryDate:       string(a.ExpiryDate),
		DrvOptionType:       opt,
		DrvStrikePrice:      float64(a.StrikePrice),
		OmsErrorDescription: string(a.ReasonDescription),
		AlgoID:              string(a.AlgoId),
		RemainingQuantity:   int(a.RemainingQuantity),
		AverageTradedPrice:  float64(a.AvgTradedPrice),
		FilledQty:           int(a.TradedQty),
	}
}
