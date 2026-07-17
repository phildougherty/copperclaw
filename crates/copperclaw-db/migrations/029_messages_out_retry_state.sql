-- M21 S3: persist delivery retry state on the outbound row itself.
--
-- Before this, retry state (attempt counter + backoff window) lived only in
-- an in-memory DashMap on the host's DeliveryService, so a host restart
-- reset MAX_DELIVERY_ATTEMPTS to "3 per host lifetime": a poisoned outbound
-- row retried unboundedly across restarts, and a row mid-backoff lost its
-- window and fired immediately on boot.
--
--   tries      -- delivery attempts already made by the host (>= 0).
--   not_before -- RFC3339 wall-clock time before which the host must not
--                 retry the row; NULL = no backoff window pending.
--
-- Host-written columns on a container-written table: the host already
-- writes messages_out rows (delivery-failure ErrorCards), and the column
-- defaults keep the container's INSERT (explicit column list) untouched.
ALTER TABLE messages_out ADD COLUMN tries INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages_out ADD COLUMN not_before TEXT;
