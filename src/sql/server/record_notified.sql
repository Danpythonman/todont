INSERT INTO
    notified
    (uuid, due, last)
VALUES
    (?1, ?2, ?3)
ON CONFLICT (uuid) DO UPDATE SET
    due  = excluded.due,
    last = excluded.last
;
