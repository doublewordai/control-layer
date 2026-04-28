// Forward-looking aliases for the planned multi-step responses dashboard.
//
// The full rename from `async-requests/` → `responses/` (per
// fusillade/docs/plans/2026-04-28-multi-step-responses.md) is deferred
// until the `/v1/responses/{id}/steps` API endpoint ships in dwctl, at
// which point this directory will gain its own implementations of the
// list, detail, per-step timeline, and recursive sub-agent tree views.
//
// In the meantime, exposing these re-exports lets new call-sites import
// from `components/features/responses` so they don't need to be rewritten
// when the move happens.

export {
    AsyncRequests as Responses,
    AsyncRequestDetail as RequestDetail,
} from "../async-requests";
