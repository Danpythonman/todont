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
    seq > ?1
ORDER BY
    seq
;
