insert into wifi_todo
    select distinct on (B.cell) mac, B.cell
    from wifi A right join cell B on A.cell = B.cell
    where not A.gone and B.last_crawled is null
on conflict do nothing;
