-- Durable retry queue for webhook deliveries.
--
-- The inline retry in `send_webhook` covers transient blips (200ms /
-- 400ms / 800ms backoff, 3 attempts) but loses everything still
-- in-flight if the process crashes between attempts. For business
-- events (budget thresholds, provider circuit openings, key expiry
-- warnings) silent loss isn't acceptable — operators rely on those
-- to drive on-call response.
--
-- Failed inline deliveries land here; a background worker drains the
-- table on a 10s tick, attempts redelivery with exponential backoff
-- (capped at 1h between attempts), and only stops retrying after
-- ~24 attempts (~1 day total). Deletion on success keeps the table
-- bounded.
--
-- Foreign key cascades on `log_forwarders.id` so deleting a forwarder
-- also drops its pending deliveries — no point retrying against a
-- destination the operator has already removed.
CREATE TABLE webhook_outbox (
    id              UUID         PRIMARY KEY DEFAULT gen_random_uuid(),
    forwarder_id    UUID         NOT NULL
                                 REFERENCES log_forwarders(id) ON DELETE CASCADE,
    payload         JSONB        NOT NULL,
    attempts        INTEGER      NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    last_error      TEXT,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Worker query: SELECT ... WHERE next_attempt_at <= now() ORDER BY
-- next_attempt_at LIMIT N. Index lets that scan stay ordered without
-- touching rows that aren't due yet.
CREATE INDEX idx_webhook_outbox_next_attempt ON webhook_outbox(next_attempt_at);
