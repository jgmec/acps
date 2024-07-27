use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use h3c::H3c;
use h3o::{CellIndex, LatLng};
use macn::Mac;
use rusqlite::{Connection, OptionalExtension};

use crate::fetch::Location;

pub struct WtfDb {
    pub main: Connection,
    cache: BTreeSet<Mac>, // macs that exist
    shards: BTreeMap<u16, Connection>,
}

impl WtfDb {
    pub fn open() -> Result<Self> {
        let main = Connection::open("main.db")?;
        main.pragma_update(None, "journal_mode", "wal")?;
        main.pragma_update(None, "synchronous", "off")?;
        main.execute_batch(include_str!("../db-main.sql"))?;

        Ok(Self {
            main,
            cache: BTreeSet::new(),
            shards: BTreeMap::new(),
        })
    }

    pub fn commit(&mut self, locs: BTreeMap<Mac, Option<Location>>) -> Result<()> {
        let mut to_cache = BTreeSet::new();
        let mut to_delete = BTreeSet::new();
        let mut to_fail = BTreeSet::new();
        let mut to_insert = BTreeMap::new();
        let mut to_insert_total = 0usize;
        for (mac, loc) in locs {
            let macn = u64::from(mac);

            if let Some(loc) = loc {
                let cell = loc.cell();
                let h3c = H3c::from(cell);

                let mut existing = self.cache.contains(&mac);
                if !existing {
                    for h3c in h3c.search_area() {
                        if self
                            .shard(h3c.0)?
                            .prepare_cached("select 1 from wifi where mac = ?")?
                            .query_row((macn,), |_| Ok(1))
                            .optional()?
                            .is_some()
                        {
                            existing = true;
                            break;
                        }
                    }
                }
                to_cache.insert(mac);

                if !existing {
                    let i = h3c.0;
                    if !to_insert.contains_key(&i) {
                        to_insert.insert(i, vec![]);
                    }
                    to_insert.get_mut(&i).unwrap().push((macn, loc));
                    to_insert_total += 1;
                }
            } else {
                to_fail.insert(macn);
            }

            to_delete.insert(macn);
        }
        info!(
            "{} new, {} failed, {} cached, {} shards open",
            to_insert_total,
            to_fail.len(),
            self.cache.len(),
            self.shards.len()
        );

        // approx 1GB of RAM
        if self.cache.len() > 32_000_000 {
            self.cache = to_cache;
            self.truncate()?;
        } else {
            self.cache.extend(to_cache);
        }

        // insert new aps in shards
        let mut to_explore = BTreeMap::new();
        for (i, ap) in to_insert {
            let tx = self.shard(i)?.transaction()?;
            {
                let mut q = tx.prepare("insert into wifi (mac, lat, lon) values (?, ?, ?)")?;
                for (mac, loc) in ap {
                    let cell = u64::from(loc.cell());
                    if !to_explore.contains_key(&cell) {
                        to_explore.insert(u64::from(cell), mac);
                    }
                    q.execute((mac, loc.lat, loc.lon))?;
                }
            }
            tx.commit()?;
        }
        info!("{} new macs to explore", to_explore.len());

        // write main state
        let tx = self.main.transaction()?;
        {
            let mut q =
                tx.prepare("insert into wifi_fail (mac) values (?) on conflict do nothing")?;
            for mac in to_fail {
                q.execute((mac,))?;
            }

            let mut q = tx.prepare("delete from wifi_brute where mac = ?")?;
            for mac in &to_delete {
                q.execute((mac,))?;
            }
            let mut q = tx.prepare("delete from wifi_explore where mac = ?")?;
            for mac in &to_delete {
                q.execute((mac,))?;
            }

            let mut q = tx.prepare(
                "insert into wifi_explore (cell, mac) values (?, ?) on conflict do nothing",
            )?;
            for (cell, mac) in to_explore {
                let loc = LatLng::from(CellIndex::try_from(cell)?);
                println!("{{\"latitude\":{},\"longitude\":{}}}", loc.lat(), loc.lng());

                q.execute((cell, mac))?;
            }
        }
        tx.commit()?;

        Ok(())
    }

    pub fn truncate(&mut self) -> Result<()> {
        for (_, shard) in &self.shards {
            shard.pragma_update(None, "wal_checkpoint", "truncate")?;
        }
        self.main
            .pragma_update(None, "wal_checkpoint", "truncate")?;

        Ok(())
    }

    fn shard(&mut self, i: u16) -> Result<&mut Connection> {
        if !self.shards.contains_key(&i) {
            let conn = Connection::open(format!("shards/{i}.db"))?;
            conn.pragma_update(None, "journal_mode", "wal")?;
            conn.pragma_update(None, "synchronous", "off")?;
            conn.execute_batch(include_str!("../db-shard.sql"))?;
            self.shards.insert(i, conn);
        }

        Ok(self.shards.get_mut(&i).unwrap())
    }
}
