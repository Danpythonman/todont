SELECT
    uuid   ,
    title  ,
    done   ,
    due    ,
    created,
    updated,
    deleted,
    remind ,
    nag
FROM
    task
WHERE
    dirty = 1
;
