-- Widen containers.status to the values the code actually writes.
-- The original dump only allowed created/running/stopped/deleted/error, but
-- the controller inserts 'deploying' and records 'failed'. Idempotent.
ALTER TABLE public.containers DROP CONSTRAINT IF EXISTS containers_status_check;
ALTER TABLE public.containers ADD CONSTRAINT containers_status_check
  CHECK (status::text = ANY (ARRAY[
    'created', 'deploying', 'running', 'stopped', 'failed', 'deleted', 'error'
  ]::text[]));
