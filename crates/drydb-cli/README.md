# drydb-cli

Command line tools for DryDB 1.4 database files: inspect the catalog, verify a file,
run queries, and build a database from a text input.

```
drydb inspect game.drydb
drydb verify game.drydb
drydb get game.drydb items 1234
drydb range game.drydb items --from 100 --to 200 --limit 20
drydb build out.drydb --table items --encoding i64 --input rows.tsv
```

Run `drydb --help` for the full list.
