package model

import (
	"encoding/json"
	"fmt"
	"net/http"
)

// Error codes returned to clients.
const (
	CodeInvalidRequest   = "invalid_request"
	CodeMarketNotAllowed = "market_order_not_allowed"
	CodeNotFound         = "not_found"
	CodeNotModifiable    = "order_not_modifiable"
	CodeUnauthorized     = "unauthorized"
	CodeBrokerAuth       = "broker_unauthorized"
	CodeBrokerRejected   = "broker_error"
	CodeRateLimited      = "rate_limited"
	CodeUpstream         = "upstream_unavailable"
	CodeConflict         = "outcome_unknown"
	CodeInternal         = "internal"
)

// APIError is a structured error returned to the client as
// {"error": {...}} with the given HTTP status.
type APIError struct {
	HTTPStatus int             `json:"-"`
	Code       string          `json:"code"`
	Message    string          `json:"message"`
	Detail     json.RawMessage `json:"detail,omitempty"` // raw broker error body, if any
}

func (e *APIError) Error() string { return e.Code + ": " + e.Message }

func Errf(status int, code, format string, a ...any) *APIError {
	return &APIError{HTTPStatus: status, Code: code, Message: fmt.Sprintf(format, a...)}
}

func BadRequest(format string, a ...any) *APIError {
	return Errf(http.StatusBadRequest, CodeInvalidRequest, format, a...)
}

func NotFound(format string, a ...any) *APIError {
	return Errf(http.StatusNotFound, CodeNotFound, format, a...)
}
