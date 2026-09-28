UPDATE task SET dirty = 1;
DELETE FROM meta WHERE key IN ('cursor', 'server_id');
