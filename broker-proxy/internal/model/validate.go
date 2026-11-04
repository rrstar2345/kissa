package model

import (
	"net/http"
	"regexp"
)

var (
	transactionTypes = set("BUY", "SELL")
	exchangeSegments = set("NSE_EQ", "NSE_FNO", "NSE_CURRENCY", "BSE_EQ", "BSE_FNO", "BSE_CURRENCY", "MCX_COMM", "IDX_I")
	productTypes     = set("CNC", "INTRADAY", "MARGIN", "MTF", "CO", "BO")
	validities       = set("DAY", "IOC")
	amoTimes         = set("PRE_OPEN", "OPEN", "OPEN_30", "OPEN_60")
	legNames         = set("ENTRY_LEG", "TARGET_LEG", "STOP_LOSS_LEG")
	correlationRe    = regexp.MustCompile(`^[a-zA-Z0-9 _-]{0,30}$`)
)

func set(v ...string) map[string]struct{} {
	m := make(map[string]struct{}, len(v))
	for _, s := range v {
		m[s] = struct{}{}
	}
	return m
}

func in(m map[string]struct{}, v string) bool { _, ok := m[v]; return ok }

// checkOrderType enforces the "no market orders" rule: brokers require a
// limit price, so only LIMIT and STOP_LOSS (stop-limit) are accepted.
func checkOrderType(t string) *APIError {
	switch t {
	case "LIMIT", "STOP_LOSS":
		return nil
	case "MARKET", "STOP_LOSS_MARKET":
		return Errf(http.StatusBadRequest, CodeMarketNotAllowed,
			"orderType %s is not allowed: orders must carry a limit price (use LIMIT or STOP_LOSS)", t)
	default:
		return BadRequest("orderType must be LIMIT or STOP_LOSS, got %q", t)
	}
}

// Validate checks a place / slice request before it reaches any broker.
func (o *OrderRequest) Validate() *APIError {
	if !in(transactionTypes, o.TransactionType) {
		return BadRequest("transactionType must be BUY or SELL")
	}
	if !in(exchangeSegments, o.ExchangeSegment) {
		return BadRequest("exchangeSegment %q is not recognised", o.ExchangeSegment)
	}
	if !in(productTypes, o.ProductType) {
		return BadRequest("productType must be one of CNC, INTRADAY, MARGIN, MTF, CO, BO")
	}
	if err := checkOrderType(o.OrderType); err != nil {
		return err
	}
	if !in(validities, o.Validity) {
		return BadRequest("validity must be DAY or IOC")
	}
	if o.SecurityID == "" {
		return BadRequest("securityId is required")
	}
	if o.Quantity <= 0 {
		return BadRequest("quantity must be > 0")
	}
	if o.DisclosedQuantity < 0 || o.DisclosedQuantity > o.Quantity {
		return BadRequest("disclosedQuantity must be between 0 and quantity")
	}
	if o.Price <= 0 {
		return BadRequest("price must be > 0 (market orders are not supported)")
	}
	if o.OrderType == "STOP_LOSS" && o.TriggerPrice <= 0 {
		return BadRequest("triggerPrice must be > 0 for STOP_LOSS")
	}
	if o.AfterMarketOrder && !in(amoTimes, o.AmoTime) {
		return BadRequest("amoTime must be PRE_OPEN, OPEN, OPEN_30 or OPEN_60 for after-market orders")
	}
	if !correlationRe.MatchString(o.CorrelationID) {
		return BadRequest("correlationId must be at most 30 chars of [a-zA-Z0-9 _-]")
	}
	return nil
}

// Validate checks a modify request.
func (m *ModifyRequest) Validate() *APIError {
	if err := checkOrderType(m.OrderType); err != nil {
		return err
	}
	if !in(validities, m.Validity) {
		return BadRequest("validity must be DAY or IOC")
	}
	if m.Price <= 0 {
		return BadRequest("price must be > 0 (market orders are not supported)")
	}
	if m.Quantity < 0 || m.DisclosedQuantity < 0 {
		return BadRequest("quantity values must not be negative")
	}
	if m.OrderType == "STOP_LOSS" && m.TriggerPrice <= 0 {
		return BadRequest("triggerPrice must be > 0 for STOP_LOSS")
	}
	if m.LegName != "" && !in(legNames, m.LegName) {
		return BadRequest("legName must be ENTRY_LEG, TARGET_LEG or STOP_LOSS_LEG")
	}
	return nil
}
