-- Release 11.12.0 initially wrote enriched records to this column. New writers
-- checkpoint raw, content-free records before enrichment; projectors accept both
-- formats while rows created by the earlier release drain during rolling upgrades.
COMMENT ON COLUMN analytics_outbox.payload IS
    'Content-free analytics payload. May contain a release-11.12.0 EnrichedRecord or a RawAnalyticsRecord; API-key bearer credentials are never stored.';
