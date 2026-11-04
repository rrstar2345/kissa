package server

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"

	"brokerproxy/internal/model"
)

// writeErr renders any error as {"error": {code, message, detail?}}.
// Unknown errors are logged and returned as a generic 500.
func writeErr(w http.ResponseWriter, err error) {
	var ae *model.APIError
	switch {
	case errors.As(err, &ae):
	case errors.Is(err, context.DeadlineExceeded):
		ae = model.Errf(http.StatusGatewayTimeout, model.CodeUpstream, "broker request timed out; the outcome may be unknown, check the order book")
	case errors.Is(err, context.Canceled):
		ae = model.Errf(http.StatusRequestTimeout, model.CodeUpstream, "request cancelled")
	default:
		if err != nil {
			slog.Error("internal error", "err", err)
		}
		ae = model.Errf(http.StatusInternalServerError, model.CodeInternal, "internal error")
	}
	writeJSON(w, ae.HTTPStatus, map[string]any{"error": ae})
}

// readJSON decodes a strict JSON body (unknown fields are rejected so a typo
// in an order field fails loudly instead of being silently ignored).
func readJSON(w http.ResponseWriter, r *http.Request, v any) *model.APIError {
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 1<<20))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return model.BadRequest("invalid JSON body: %v", err)
	}
	if _, err := dec.Token(); err != io.EOF {
		return model.BadRequest("invalid JSON body: trailing data")
	}
	return nil
}
