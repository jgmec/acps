drop table wifi_explore;

create table wifi_explore (
    cell bigint unsigned not null primary key,
    mac binary(6) not null
);
