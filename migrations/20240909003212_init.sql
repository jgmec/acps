create table wifi (
    part bigint not null,
    mac macaddr not null,
    primary key (part, mac),

    cell bigint not null,
    lat double precision not null,
    lon double precision not null,
    gone boolean not null default false
) partition by hash (part);

create table wifi_0 partition of wifi for values with (modulus 16, remainder 0);
create table wifi_1 partition of wifi for values with (modulus 16, remainder 1);
create table wifi_2 partition of wifi for values with (modulus 16, remainder 2);
create table wifi_3 partition of wifi for values with (modulus 16, remainder 3);
create table wifi_4 partition of wifi for values with (modulus 16, remainder 4);
create table wifi_5 partition of wifi for values with (modulus 16, remainder 5);
create table wifi_6 partition of wifi for values with (modulus 16, remainder 6);
create table wifi_7 partition of wifi for values with (modulus 16, remainder 7);
create table wifi_8 partition of wifi for values with (modulus 16, remainder 8);
create table wifi_9 partition of wifi for values with (modulus 16, remainder 9);
create table wifi_10 partition of wifi for values with (modulus 16, remainder 10);
create table wifi_11 partition of wifi for values with (modulus 16, remainder 11);
create table wifi_12 partition of wifi for values with (modulus 16, remainder 12);
create table wifi_13 partition of wifi for values with (modulus 16, remainder 13);
create table wifi_14 partition of wifi for values with (modulus 16, remainder 14);
create table wifi_15 partition of wifi for values with (modulus 16, remainder 15);

create index wifi_cell on wifi (cell);
create index wifi_gone on wifi (mac) where gone;

create table todo (
    mac macaddr primary key not null,
    cell bigint
);
create index todo_cell on todo (cell);

create table cell (
    part bigint not null,
    cell bigint not null,
    primary key (part, cell),

    last_crawled timestamp with time zone
);

create index cell_todo on cell (part, cell) where last_crawled is null;
