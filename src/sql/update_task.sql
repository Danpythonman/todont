UPDATE
    task
SET
    title   = ?2,
    done    = ?3,
    due     = ?4,
    updated = ?5,
    remind  = ?6,
    nag     = ?7,
    dirty   = 1
WHERE
    id = ?1
    AND deleted = 0
;
