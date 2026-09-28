-- Last write wins; ?10 is the change's new seq.
INSERT INTO
    task
    (uuid, title, done, due, created, updated, deleted, remind, nag, seq)
VALUES
    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT (uuid) DO UPDATE SET
    title   = excluded.title  ,
    done    = excluded.done   ,
    due     = excluded.due    ,
    created = excluded.created,
    updated = excluded.updated,
    deleted = excluded.deleted,
    remind  = excluded.remind ,
    nag     = excluded.nag    ,
    seq     = excluded.seq
WHERE
    excluded.updated > task.updated
;
