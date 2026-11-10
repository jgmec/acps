-- The area crawl per radio technology: LTE and 5G NR replies both return nearby cells, so both
-- are crawled by area, and an area crawled for one still needs crawling for the other. Every
-- existing row is LTE. ('gsm' cells aren't crawled by area: Apple's GSM replies have no
-- nearby cells.)
alter table cell_crawl add column radio text not null default 'lte';
alter table cell_crawl drop constraint cell_crawl_pkey, add primary key (mcc, mnc, area, radio);
drop index cell_crawl_todo;
create index cell_crawl_todo on cell_crawl (mcc, mnc, area, radio) where last_crawled is null;
