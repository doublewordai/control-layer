-- An organisation's cache-assurance terms on one model, written by the org overlay
-- catalog (`cache_assurance`, next to `cache_tariff`) and read by the cache layer:
-- unmarked requests from the organisation are billed part of the gap between the
-- engine's cache hit and the reference-cache estimate. JSON, one of
-- {"gap_share": α} or {"prompt_floor": X}, each in [0, 1]. NULL = no terms (the
-- organisation is billed the engine's hit, as everyone else). Additive nullable
-- column: the previous release neither reads nor writes it.
SET LOCAL lock_timeout = '5s';

ALTER TABLE model_overlays ADD COLUMN IF NOT EXISTS cache_assurance JSONB;
