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
    deleted = 0
    AND (?1 OR done = 0)
ORDER BY
    done,
    due IS NULL,
    due,
    created
;
