drop index wifi_cell;
drop index wifi_first_seen;
drop index wifi_last_crawled;

create index wifi_to_crawl on wifi (cell) where last_crawled is null;
