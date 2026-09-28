SELECT
    id     ,
    uuid   ,
    title  ,
    done   ,
    due    ,
    created,
    updated,
    remind ,
    nag    ,
    dirty
FROM
    task
WHERE
    id = ?1
    AND deleted = 0
;
