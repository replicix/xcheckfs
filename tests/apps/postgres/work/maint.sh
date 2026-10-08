#!/usr/bin/env bash
# Periodic maintenance while the churn workload runs (as user postgres, inside
# the container): file rewrite+rename+unlink (VACUUM FULL, REINDEX), checkpoints,
# index create/drop, table create/truncate/drop, WAL switches.
# Usage: maint.sh SECONDS
set -u
end=$((SECONDS + ${1:?seconds}))
q() {
    local t0=$SECONDS
    psql -X -q -U postgres -d postgres -c "$1" >/dev/null 2>&1 || echo "maint: FAILED: $1"
    echo "maint: $((SECONDS - t0))s $1"
}
while ((SECONDS < end)); do
    q "VACUUM FULL pgbench_history"
    q "CHECKPOINT"
    q "CREATE INDEX CONCURRENTLY xc_idx_h ON pgbench_history (aid, tid)"
    q "DROP INDEX xc_idx_h"
    q "CREATE TABLE xc_bulk AS SELECT g AS id, repeat(md5(g::text), 4) AS pad FROM generate_series(1, 150000) g"
    q "DELETE FROM xc_bulk WHERE id > 10000"
    q "VACUUM xc_bulk"
    q "SELECT pg_switch_wal()"
    q "DROP TABLE xc_bulk"
    q "REINDEX TABLE CONCURRENTLY xc_churn"
    q "VACUUM FULL xc_churn"
    q "CHECKPOINT"
    sleep 3
done
