-- Last write wins: a remote change only lands if it is newer.
INSERT INTO
    task
    (uuid, title, done, due, created, updated, deleted, remind, nag, dirty)
VALUES
    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)
ON CONFLICT (uuid) DO UPDATE SET
    title   = excluded.title  ,
    done    = excluded.done   ,
    due     = excluded.due    ,
    created = excluded.created,
    updated = excluded.updated,
    deleted = excluded.deleted,
    remind  = excluded.remind ,
    nag     = excluded.nag    ,
    dirty   = 0
WHERE
    excluded.updated > task.updated
;
