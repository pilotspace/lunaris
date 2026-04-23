-- Pre-create the three pgmq queues + the AGE graph used by Lunaris.
-- Idempotent: pgmq.create returns silently when the queue already exists; AGE create_graph
-- raises if the graph exists, so we wrap in DO $$ ... $$.

SELECT pgmq.create('__lunaris_verify__');
SELECT pgmq.create('__lunaris_consolidate__');
SELECT pgmq.create('__lunaris_audit__');

DO $$
BEGIN
  PERFORM ag_catalog.create_graph('lunaris_graph');
EXCEPTION WHEN OTHERS THEN
  -- already exists or AGE not loaded in this session — both are tolerated at migration time
  RAISE NOTICE 'lunaris_graph create_graph noop: %', SQLERRM;
END $$;
