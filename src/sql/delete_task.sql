UPDATE
    task
SET
    deleted = 1 ,
    updated = ?2,
    dirty   = 1
WHERE
    id = ?1
    AND deleted = 0
;
