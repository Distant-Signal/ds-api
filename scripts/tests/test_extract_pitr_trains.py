# ruff: noqa: D101, D102, S603, S607, FBT001, FBT002, S608
# Test method names are their documentation; the fixed argv are local tools
# (PostgreSQL) on throwaway data; booleans in fixtures are literals.
"""Tests for scripts/extract-pitr-trains.py.

uv run python -m unittest discover -s scripts/tests

Two throwaway databases on one local server stand in for the PITR scratch
and the live database; both get the real DDL of the touched tables
(fixtures/pitr-train-schema.sql, pg_dump -s of ds-core's migrations at
20261120130200). The key test applies the emitted batch to the live database
and compares every table row for row (`to_jsonb`) with the scratch.
"""

import contextlib
import datetime as dt
import importlib.util
import io
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from typing import override
from unittest import mock

HERE = Path(__file__).resolve().parent
SCRIPT = HERE.parent / "extract-pitr-trains.py"
SCHEMA = HERE / "fixtures" / "pitr-train-schema.sql"
RUN_AT = dt.datetime(2026, 11, 22, 12, 0, 0, tzinfo=dt.UTC)


def load_script() -> ModuleType:
    """Import the hyphenated script as a module."""
    spec = importlib.util.spec_from_file_location("extract_pitr_trains", SCRIPT)
    if spec is None or spec.loader is None:
        raise ImportError(SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules["extract_pitr_trains"] = module
    spec.loader.exec_module(module)
    return module


ex = load_script()

HOSTILE_UID = 'C\'9$ds0$\\ "q" $$ \u00e9\u4e2d'
HOSTILE_TEXT = "pl'$ds1$ \\n\nline2 \t \\u0041 $$"

# Scratch: three trains. 101 is plain, 102 has hostile strings and
# microsecond/offset timestamps and every child, 103 has no children at all.
SCRATCH_SEED = f"""
INSERT INTO trains (id, train_uid, service_date, origin_crs, scheduled_departure,
    destination_crs, calling_points, matched_line_id, schedule_matched_at,
    train_id, resolved_at, created_at, skipped_stations, platform,
    planned_platform, origin_name, destination_name) VALUES
 (101, 'C10101', '2026-10-10', 'PAD', '2026-10-10 09:00:00+01', 'SWI',
  '[{{"tiploc":"PADTON","kind":"Origin"}}]', 'gwr', '2026-10-10 06:00:00.123456+00',
  '1A01', '2026-10-10 06:01:00+00', '2026-10-09 22:00:00.5+00',
  '{{RDG,"X Y"}}', '3', NULL, 'Paddington', 'Swindon'),
 (102, $u${HOSTILE_UID}$u$, '2026-10-11', 'KGX', '2026-10-11 10:00:00+01', 'EDB',
  '{{"a":[1,2,{{"b":"\\u00e9 $$ ''"}}],"n":null}}', NULL,
  '2026-10-11 09:00:00.000001+05:30', NULL, NULL, '2026-10-10 23:00:00-08',
  '{{}}', $u${HOSTILE_TEXT}$u$, NULL, NULL, NULL),
 (103, 'C10303', '2026-10-19', 'EUS', NULL, NULL, NULL, NULL, NULL, '1B03',
  NULL, '2026-10-18 12:00:00+00', '{{}}', NULL, NULL, NULL, NULL);
INSERT INTO train_movement_events (id, dedup_key, msg_type, event_type, loc_stanox,
    loc_crs, planned_timestamp, actual_timestamp, variation_status, raw_body,
    received_at, trains_id, gbtt_timestamp, platform) VALUES
 (5001, 'k1', '0003', 'DEPARTURE', '12345', 'PAD', '2026-10-10 09:00:00+01',
  '2026-10-10 09:01:30.25+01', 'LATE', '{{"x":1}}', '2026-10-10 08:01:31.5+00',
  101, '2026-10-10 09:00:00+01', '3'),
 (5002, 'k2', '0003', 'ARRIVAL', NULL, NULL, NULL, NULL, NULL, '[]',
  '2026-10-10 10:00:00+00', 101, NULL, NULL),
 (5003, $k${HOSTILE_TEXT}$k$, '0003', 'ARRIVAL', 'x', 'KGX', NULL, NULL, 'ON TIME',
  '{{"s":"$$ \\\\ ''q''","u":"\\u0041"}}', '2026-10-11 10:00:00+00', 102, NULL,
  $p${HOSTILE_TEXT}$p$);
INSERT INTO train_current_state (status, last_reported_location, last_event_type,
    delay_minutes, next_calling_point, eta_next, eta_source, updated_at, id,
    trains_id, event_time) VALUES
 ('en_route', 'RDG', 'DEPARTURE', 2, 'DID', '2026-10-10 09:40:00.000123+01',
  'darwin-estimated', '2026-10-10 09:20:00+01', 7001, 101,
  '2026-10-10 09:19:00+01'),
 ('completed', NULL, NULL, NULL, NULL, NULL, NULL, '2026-10-11 11:00:00+00', 7002,
  102, NULL);
INSERT INTO train_reasons (trains_id, msg_type, reason_code, canx_type, loc_stanox,
    event_at, received_at) VALUES
 (101, '0002', 'IA', 'ON CALL', '12345', '2026-10-10 09:05:00+01',
  '2026-10-10 08:06:00+00'),
 (101, '0005', 'XB', NULL, NULL, NULL, '2026-10-10 08:07:00+00');
INSERT INTO train_service_labels (trains_id, service_class, toc_id) VALUES
 (101, 'passenger_express', 'GW'), (102, 'unknown', NULL);
"""

# Live: a high-id train that stands in for everything since, the subscriptions
# (one per leg, plus a second leg on train 101), none with a train.
LIVE_SEED = """
INSERT INTO users VALUES ('u1');
INSERT INTO trains (id, train_uid, service_date) VALUES (900000, 'LIVE1', '2026-11-20');
INSERT INTO train_movement_events (id, dedup_key, msg_type, raw_body, trains_id)
    VALUES (950000, 'live', '0003', '{}', 900000);
INSERT INTO train_current_state (id, trains_id) VALUES (970000, 900000);
INSERT INTO train_subscriptions (id, user_id, service_date) VALUES
    (1, 'u1', '2026-10-10'), (2, 'u1', '2026-10-10'), (3, 'u1', '2026-10-11'),
    (4, 'u1', '2026-10-19');
"""

TABLE_ORDER = {
    "trains": "id",
    "train_movement_events": "id",
    "train_current_state": "id",
    "train_reasons": "trains_id, msg_type",
    "train_service_labels": "trains_id",
}
TRAIN_IDS = "(101, 102, 103)"


class PostgresCase(unittest.TestCase):
    """A throwaway server with a scratch and a live database."""

    data: str
    sock: str

    @override
    @classmethod
    def setUpClass(cls) -> None:
        cls.data = tempfile.mkdtemp(prefix="pgdata")
        # a short path: unix socket paths are limited to about 100 bytes
        cls.sock = tempfile.mkdtemp(prefix="pg", dir="/tmp")
        cls.addClassCleanup(shutil.rmtree, cls.data, ignore_errors=True)
        cls.addClassCleanup(shutil.rmtree, cls.sock, ignore_errors=True)
        subprocess.run(
            ["initdb", "-D", cls.data, "-A", "trust", "-U", "t", "-E", "UTF8"],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            [
                "pg_ctl",
                "-D",
                cls.data,
                "-l",
                f"{cls.sock}/server.log",
                "-o",
                f"-k {cls.sock} -c listen_addresses='' -c logging_collector=off",
                "-w",
                "start",
            ],
            check=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for db in ("scratch", "live"):
            cls.psql("postgres", f"CREATE DATABASE {db}")
            cls.psql(db, SCHEMA.read_text(encoding="utf-8"))
        cls.psql("scratch", SCRATCH_SEED)

    @override
    @classmethod
    def tearDownClass(cls) -> None:
        subprocess.run(
            ["pg_ctl", "-D", cls.data, "-m", "immediate", "stop"],
            check=False,
            capture_output=True,
        )

    @classmethod
    def psql(cls, db: str, sql: str) -> str:
        done = subprocess.run(
            [
                "psql",
                "-X",
                "-At",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                cls.sock,
                "-U",
                "t",
                "-d",
                db,
            ],
            input=sql,
            capture_output=True,
            text=True,
            check=False,
        )
        if done.returncode != 0:
            raise AssertionError(done.stderr)
        return done.stdout

    @classmethod
    def psql_cmd(cls, db: str = "scratch") -> str:
        return f"psql -X -At -h {cls.sock} -U t -d {db}"

    @override
    def setUp(self) -> None:
        self.psql(
            "live",
            "TRUNCATE trains, train_subscriptions, users CASCADE;" + LIVE_SEED,
        )
        self.tmp = Path(tempfile.mkdtemp(prefix="pitr"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    # helpers -------------------------------------------------------------

    def targets(self, *items: tuple[int, str, str, str]) -> Path:
        path = self.tmp / "targets.jsonl"
        path.write_text(
            "".join(
                json.dumps(
                    {"subscriptionId": s, "trainUid": u, "serviceDate": d, "target": t}
                )
                + "\n"
                for s, u, d, t in items
            ),
            encoding="utf-8",
        )
        return path

    def default_targets(self) -> Path:
        return self.targets(
            (1, "C10101", "2026-10-10", "T1"),
            (2, "C10101", "2026-10-10", "T1"),
            (3, HOSTILE_UID, "2026-10-11", "T1"),
            (4, "C10303", "2026-10-19", "T1"),
            (99, "OTHER", "2026-10-19", "T2"),
        )

    def run_cli(self, *argv: str) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(ex.main(list(argv)), 0)
        return out.getvalue()

    def extract(self, targets: Path | None = None, emit: bool = True) -> Path:
        argv = [
            "extract",
            "--targets",
            str(targets or self.default_targets()),
            "--target",
            "T1",
            "--psql",
            self.psql_cmd(),
            "--out",
            str(self.tmp / "extract.jsonl"),
        ]
        if emit:
            argv += ["--emit-sql", str(self.tmp / "sql")]
        self.run_cli(*argv)
        return self.tmp / "sql"

    def apply(self, sql_dir: Path, name: str = "batch-0001.sql") -> str:
        return self.psql("live", (sql_dir / name).read_text(encoding="utf-8"))

    def rows(self, db: str, table: str, where: str = "true") -> list[object]:
        order = TABLE_ORDER[table]
        text = self.psql(
            db,
            f"SET TimeZone = 'UTC'; SELECT to_jsonb(x) FROM {table} x "
            f"WHERE {where} ORDER BY {order};",
        )
        return [json.loads(line) for line in text.splitlines() if line.startswith("{")]

    def verify(self, sql_dir: Path) -> dict[str, int]:
        text = self.psql("live", (sql_dir / "verify.sql").read_text(encoding="utf-8"))
        line = next(line for line in text.splitlines() if "|" in line)
        names = [
            "trains",
            "events",
            "state",
            "reasons",
            "labels",
            "subscriptions",
        ]
        values = [int(v) for v in line.split("|")]
        out: dict[str, int] = {}
        for i, name in enumerate(names):
            out[f"{name}_expected"] = values[2 * i]
            out[f"{name}_ok"] = values[2 * i + 1]
        return out

    def assert_all_restored(self, sql_dir: Path) -> None:
        for table in TABLE_ORDER:
            where = (
                f"id IN {TRAIN_IDS}"
                if table == "trains"
                else f"trains_id IN {TRAIN_IDS}"
            )
            self.assertEqual(
                self.rows("live", table, where),
                self.rows("scratch", table, where),
                table,
            )
        v = self.verify(sql_dir)
        for name in ("trains", "events", "state", "reasons", "labels", "subscriptions"):
            self.assertEqual(v[f"{name}_expected"], v[f"{name}_ok"], name)


class ExtractTests(PostgresCase):
    def test_normal_extract_applies_and_reads_back_row_for_row(self) -> None:
        sql = self.extract()
        counts = json.loads((sql / "manifest.json").read_text())["toInsert"]
        self.assertEqual(
            counts,
            {
                "trains": 3,
                "train_movement_events": 3,
                "train_current_state": 2,
                "train_reasons": 2,
                "train_service_labels": 2,
                "subscriptions": 4,
            },
        )
        self.apply(sql)
        self.assert_all_restored(sql)  # the key test: row for row
        linked = self.psql(
            "live", "SELECT id, trains_id FROM train_subscriptions ORDER BY id"
        )
        self.assertEqual(linked.split(), ["1|101", "2|101", "3|102", "4|103"])
        # live rows that were already there are untouched
        self.assertEqual(
            self.psql("live", "SELECT train_uid FROM trains WHERE id = 900000").strip(),
            "LIVE1",
        )

    def test_other_target_lines_are_ignored(self) -> None:
        self.extract(emit=False)
        header, bundles = ex.read_extract(self.tmp / "extract.jsonl")
        self.assertEqual(header["target"], "T1")
        self.assertEqual(len(bundles), 3)
        self.assertEqual(header["counts"]["targets"], 4)

    def test_idempotent_rerun(self) -> None:
        sql = self.extract()
        self.apply(sql)
        before = {t: self.rows("live", t) for t in TABLE_ORDER}
        self.apply(sql)
        self.assertEqual(before, {t: self.rows("live", t) for t in TABLE_ORDER})
        self.assert_all_restored(sql)

    def test_batch_is_one_transaction_with_timeouts(self) -> None:
        sql = self.extract()
        text = (sql / "batch-0001.sql").read_text()
        self.assertTrue(text.startswith("BEGIN;\nSET LOCAL lock_timeout"))
        self.assertIn("SET LOCAL statement_timeout", text)
        self.assertTrue(text.rstrip().endswith("COMMIT;"))
        self.assertNotIn("DELETE", text)
        self.assertNotIn("setval", text)

    def test_batch_size_splits_transactions(self) -> None:
        self.extract(emit=False)
        header, bundles = ex.read_extract(self.tmp / "extract.jsonl")
        texts = ex.batches(bundles, 1)
        self.assertEqual(len(texts), 3)
        for text in texts:
            self.psql("live", text)
        self.assertEqual(
            self.psql("live", "SELECT count(*) FROM trains WHERE id < 900000").strip(),
            "3",
        )
        self.assertEqual(header["target"], "T1")

    def test_zero_match_is_skipped_and_counted(self) -> None:
        targets = self.targets(
            (1, "C10101", "2026-10-10", "T1"),
            (2, "NOPE", "2026-10-10", "T1"),
            (3, "C10101", "2026-10-11", "T1"),  # right uid, wrong date
        )
        out = self.run_cli(
            "extract",
            "--targets",
            str(targets),
            "--target",
            "T1",
            "--psql",
            self.psql_cmd(),
            "--out",
            str(self.tmp / "e.jsonl"),
        )
        self.assertIn("no_match                2", out)
        self.assertIn("extracted               1", out)
        self.assertNotIn("NOPE", out)
        self.assertNotIn("C10101", out)

    def test_several_matches_are_skipped_not_guessed(self) -> None:
        line = self.psql(
            "scratch",
            "SELECT jsonb_build_object('idx', 0, 'train', to_jsonb(t), "
            "'events', '[]'::jsonb, 'state', NULL, 'reasons', '[]'::jsonb, "
            "'label', NULL)::text FROM trains t WHERE id = 101",
        ).strip()
        with mock.patch.object(ex, "run_psql", return_value=f"SET\n{line}\n{line}\n"):
            result = ex.extract_target(
                [ex.Target(1, "C10101", dt.date(2026, 10, 10), "T1")],
                "unused",
                verbose=False,
                target="T1",
            )
        self.assertEqual(result.counts["ambiguous"], 1)
        self.assertEqual(result.bundles, [])

    def test_nul_byte_rejects_the_train(self) -> None:
        line = json.loads(
            self.psql(
                "scratch",
                "SELECT jsonb_build_object('idx', 0, 'train', to_jsonb(t), "
                "'events', '[]'::jsonb, 'state', NULL, 'reasons', '[]'::jsonb, "
                "'label', NULL)::text FROM trains t WHERE id = 101",
            )
        )
        line["train"]["platform"] = "3\x00"
        with mock.patch.object(ex, "run_psql", return_value=json.dumps(line)):
            result = ex.extract_target(
                [ex.Target(1, "C10101", dt.date(2026, 10, 10), "T1")],
                "unused",
                verbose=False,
                target="T1",
            )
        self.assertEqual(result.counts["rejected"], 1)

    def test_schema_drift_rejects_the_train(self) -> None:
        line = json.loads(
            self.psql(
                "scratch",
                "SELECT jsonb_build_object('idx', 0, 'train', to_jsonb(t), "
                "'events', '[]'::jsonb, 'state', NULL, 'reasons', '[]'::jsonb, "
                "'label', NULL)::text FROM trains t WHERE id = 101",
            )
        )
        line["train"]["new_column"] = 1
        with mock.patch.object(ex, "run_psql", return_value=json.dumps(line)):
            result = ex.extract_target(
                [ex.Target(1, "C10101", dt.date(2026, 10, 10), "T1")],
                "unused",
                verbose=False,
                target="T1",
            )
        self.assertEqual(result.counts["rejected"], 1)

    def test_scratch_session_is_read_only(self) -> None:
        script = ex.scratch_script([("C10101", dt.date(2026, 10, 10))])
        self.assertTrue(script.startswith("SET default_transaction_read_only = on;"))
        self.assertIn("BEGIN READ ONLY;", script)
        with self.assertRaises(AssertionError):
            self.psql(
                "scratch",
                "SET default_transaction_read_only = on;\nDELETE FROM trains;",
            )

    def test_output_has_no_ids_and_files_are_private(self) -> None:
        sql = self.extract()
        manifest = (sql / "manifest.json").read_text()
        for needle in ("C10101", "101", "5001", "uid", '"id"', "serviceDate"):
            self.assertNotIn(needle, manifest.replace("sha256", ""), needle)
        for path in [*sql.iterdir(), self.tmp / "extract.jsonl"]:
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600, path.name)
        files = {f["name"] for f in json.loads(manifest)["files"]}
        self.assertEqual(
            files,
            {"batch-0001.sql", "preflight.sql", "verify.sql", "rollback.sql"},
        )


class GuardTests(PostgresCase):
    def test_id_collision_with_other_content_is_a_noop_for_that_train(self) -> None:
        sql = self.extract()
        self.psql(
            "live",
            "INSERT INTO trains (id, train_uid, service_date) "
            "VALUES (102, 'SOMETHING-ELSE', '2026-11-01');",
        )
        self.apply(sql)
        self.assertEqual(
            self.psql("live", "SELECT train_uid FROM trains WHERE id = 102").strip(),
            "SOMETHING-ELSE",
        )
        self.assertEqual(
            self.psql(
                "live", "SELECT count(*) FROM train_current_state WHERE trains_id = 102"
            ).strip(),
            "0",
        )
        # the other trains went in; subscription 3 stayed unlinked
        self.assertEqual(
            self.psql(
                "live", "SELECT id, trains_id FROM train_subscriptions ORDER BY id"
            ).split(),
            ["1|101", "2|101", "3|", "4|103"],
        )
        v = self.verify(sql)
        self.assertEqual((v["trains_expected"], v["trains_ok"]), (3, 2))

    def test_existing_live_row_for_same_uid_and_date_is_not_duplicated(self) -> None:
        sql = self.extract()
        self.psql(
            "live",
            "INSERT INTO trains (id, train_uid, service_date) "
            "VALUES (910000, 'C10101', '2026-10-10');",
        )
        self.apply(sql)
        self.assertEqual(
            self.psql(
                "live", "SELECT count(*) FROM trains WHERE train_uid = 'C10101'"
            ).strip(),
            "1",
        )
        self.assertEqual(
            self.psql(
                "live",
                "SELECT count(*) FROM train_movement_events WHERE trains_id = 101",
            ).strip(),
            "0",
        )
        # no relink to the restored id, and none to the live row either
        self.assertEqual(
            self.psql(
                "live", "SELECT id, trains_id FROM train_subscriptions ORDER BY id"
            ).split(),
            ["1|", "2|", "3|102", "4|103"],
        )

    def test_existing_live_row_for_same_train_id_and_date_is_not_duplicated(
        self,
    ) -> None:
        sql = self.extract()
        self.psql(
            "live",
            "INSERT INTO trains (id, train_uid, service_date, train_id) "
            "VALUES (910000, 'OTHERUID', '2026-10-10', '1A01');",
        )
        self.apply(sql)
        self.assertEqual(
            self.psql(
                "live", "SELECT count(*) FROM trains WHERE service_date = '2026-10-10'"
            ).strip(),
            "1",
        )

    def test_taken_event_id_skips_the_train(self) -> None:
        sql = self.extract()
        self.psql(
            "live",
            "INSERT INTO train_movement_events (id, dedup_key, msg_type, raw_body, "
            "trains_id) VALUES (5003, 'z', '0003', '{}', 900000);",
        )
        self.apply(sql)
        self.assertEqual(
            self.psql("live", "SELECT count(*) FROM trains WHERE id = 102").strip(), "0"
        )
        self.assertEqual(
            self.psql(
                "live", "SELECT dedup_key FROM train_movement_events WHERE id = 5003"
            ).strip(),
            "z",
        )

    def test_relink_guards(self) -> None:
        sql = self.extract()
        self.psql(
            "live",
            "UPDATE train_subscriptions SET trains_id = 900000 WHERE id = 1;"
            "UPDATE train_subscriptions SET service_date = '2026-10-12' WHERE id = 3;",
        )
        self.apply(sql)
        self.assertEqual(
            self.psql(
                "live", "SELECT id, trains_id FROM train_subscriptions ORDER BY id"
            ).split(),
            ["1|900000", "2|101", "3|", "4|103"],
        )

    def test_missing_subscription_is_harmless(self) -> None:
        sql = self.extract()
        self.psql("live", "DELETE FROM train_subscriptions WHERE id = 4;")
        self.apply(sql)
        self.assertEqual(
            self.psql("live", "SELECT count(*) FROM trains WHERE id = 103").strip(), "1"
        )

    def test_ids_not_above_restored_abort_the_whole_batch(self) -> None:
        sql = self.extract()
        self.psql("live", "DELETE FROM trains WHERE id = 900000;")
        done = subprocess.run(
            [
                "psql",
                "-X",
                "-At",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                self.sock,
                "-U",
                "t",
                "-d",
                "live",
                "-f",
                str(sql / "batch-0001.sql"),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("not above", done.stderr)
        self.assertEqual(self.psql("live", "SELECT count(*) FROM trains").strip(), "0")

    def test_changed_live_schema_aborts_the_batch(self) -> None:
        sql = self.extract()
        self.psql("live", "ALTER TABLE trains ADD COLUMN extra text;")
        self.addCleanup(self.psql, "live", "ALTER TABLE trains DROP COLUMN extra;")
        done = subprocess.run(
            [
                "psql",
                "-X",
                "-At",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                self.sock,
                "-U",
                "t",
                "-d",
                "live",
                "-f",
                str(sql / "batch-0001.sql"),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("schema differs", done.stderr)

    def test_rollback_removes_what_the_batch_added(self) -> None:
        sql = self.extract()
        self.apply(sql)
        self.psql("live", (sql / "rollback.sql").read_text())
        self.assertEqual(
            self.psql("live", "SELECT count(*) FROM trains WHERE id < 900000").strip(),
            "0",
        )
        self.assertEqual(
            self.psql(
                "live", "SELECT count(*) FROM train_movement_events WHERE id < 900000"
            ).strip(),
            "0",
        )
        self.assertEqual(
            self.psql(
                "live",
                "SELECT count(*) FROM train_subscriptions WHERE trains_id IS NOT NULL",
            ).strip(),
            "0",
        )
        self.assertEqual(
            self.psql("live", "SELECT count(*) FROM trains WHERE id = 900000").strip(),
            "1",
        )


class PreflightTests(PostgresCase):
    def preflight(self, sql_dir: Path) -> tuple[list[int], list[str]]:
        text = self.psql("live", (sql_dir / "preflight.sql").read_text())
        lines = [line for line in text.splitlines() if "|" in line]
        return [int(v) for v in lines[0].split("|")], lines[1].split("|")

    def test_clean_live_database(self) -> None:
        sql = self.extract()
        counts, ok = self.preflight(sql)
        # trains, already_restored, id_collision, live_row_exists, child_id_taken,
        # subscriptions, relinkable, already_linked, missing
        self.assertEqual(counts, [3, 0, 0, 0, 0, 4, 4, 0, 0])
        self.assertEqual(set(ok), {"t"})

    def test_counts_each_hazard(self) -> None:
        sql = self.extract()
        self.psql(
            "live",
            "INSERT INTO trains (id, train_uid, service_date) VALUES "
            "(102, 'X', '2026-11-01'), (910000, 'C10101', '2026-10-10');"
            "INSERT INTO train_movement_events (id, dedup_key, msg_type, raw_body) "
            "VALUES (5002, 'q', '0003', '{}');"
            "UPDATE train_subscriptions SET trains_id = 900000 WHERE id = 4;"
            "DELETE FROM train_subscriptions WHERE id = 3;",
        )
        counts, _ = self.preflight(sql)
        self.assertEqual(counts[:5], [3, 0, 1, 1, 1])  # categories overlap
        self.assertEqual(counts[5:], [4, 2, 1, 1])

    def test_after_the_batch_everything_is_already_restored(self) -> None:
        sql = self.extract()
        self.apply(sql)
        counts, _ = self.preflight(sql)
        self.assertEqual(counts[:5], [3, 3, 0, 0, 0])
        self.assertEqual(counts[5:], [4, 0, 4, 0])

    def test_headroom_flag_fails_when_live_ids_are_low(self) -> None:
        sql = self.extract()
        self.psql("live", "TRUNCATE trains CASCADE;")
        _, ok = self.preflight(sql)
        self.assertEqual(ok[:3], ["f", "f", "f"])

    def test_preflight_subcommand_writes_private_file(self) -> None:
        self.extract(emit=False)
        out = self.tmp / "pre.sql"
        printed = self.run_cli(
            "preflight", "--extract", str(self.tmp / "extract.jsonl"), "--out", str(out)
        )
        self.assertNotIn("C10101", printed)
        self.assertEqual(stat.S_IMODE(out.stat().st_mode), 0o600)
        self.assertEqual(
            out.read_text(), (self.emit_dir() / "preflight.sql").read_text()
        )

    def emit_dir(self) -> Path:
        self.run_cli(
            "emit",
            "--extract",
            str(self.tmp / "extract.jsonl"),
            "--emit-sql",
            str(self.tmp / "sql2"),
        )
        return self.tmp / "sql2"

    def test_plan_prints_counts_only(self) -> None:
        self.extract(emit=False)
        out = self.run_cli("plan", "--extract", str(self.tmp / "extract.jsonl"))
        self.assertIn("would insert train_movement_events", out)
        for needle in ("C10101", "5001", HOSTILE_UID):
            self.assertNotIn(needle, out)


class HostileStringTests(unittest.TestCase):
    def test_dollar_quote_avoids_tags_in_the_text(self) -> None:
        for text in ("a$ds0$b", "x$ds0", "$ds0$ $ds1$", ""):
            quoted = ex.dollar_quote(text)
            tag = quoted[: quoted.index("$", 1) + 1]
            self.assertEqual(quoted, f"{tag}{text}{tag}")
            self.assertNotIn(tag, text)

    def test_nul_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            ex.dollar_quote("a\x00b")
        self.assertTrue(ex.has_nul({"a": ["b", {"c\x00": 1}]}))
        self.assertFalse(ex.has_nul({"a": "\\u0000"}))

    def test_target_validation(self) -> None:
        good = {
            "subscriptionId": 1,
            "trainUid": "C1",
            "serviceDate": "2026-10-10",
            "target": "T1",
        }
        self.assertEqual(ex.parse_target(good).subscription_id, 1)
        for patch in (
            {"subscriptionId": 0},
            {"subscriptionId": True},
            {"trainUid": ""},
            {"trainUid": "a\x00"},
            {"serviceDate": "10/10/2026"},
            {"target": "T3"},
        ):
            with self.assertRaises(ValueError):
                ex.parse_target({**good, **patch})
        with self.assertRaises(ValueError):
            ex.parse_target([])

    def test_psql_failure_does_not_echo_sql(self) -> None:
        with self.assertRaises(SystemExit) as caught:
            ex.run_psql("false", "SELECT 'secret'", verbose=False)
        self.assertNotIn("secret", str(caught.exception))


if __name__ == "__main__":
    os.environ.setdefault("PYTHONHASHSEED", "0")
    unittest.main()
