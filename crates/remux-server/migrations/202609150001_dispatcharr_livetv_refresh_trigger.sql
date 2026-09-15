-- RefreshDispatcharrLiveTv had no recurring trigger, so channel/stream titles
-- (which embed a resolution tag computed from Dispatcharr's stream_stats at
-- sync time) never refreshed after the first manual run, even as the
-- underlying stream health drifted.
INSERT OR IGNORE INTO task_triggers (id, task_id, kind, time_limit_hours, cron)
VALUES ('default-dispatcharrlivetv-interval', 'RefreshDispatcharrLiveTv',
        'IntervalTrigger', NULL, '0 0 */12 * * *');
