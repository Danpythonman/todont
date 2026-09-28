-- Only if unchanged since it was pushed; a newer local edit stays dirty.
UPDATE
    task
SET
    dirty = 0
WHERE
    uuid = ?1
    AND updated = ?2
;
