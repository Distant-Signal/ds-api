# ruff: noqa: D102, S101, S603, S607, FBT001, FBT002, FBT003
# Test method names are their documentation; the fixed argv are local tools
# (zstd, PostgreSQL) on throwaway data; booleans in fixtures are literals.
"""Tests for scripts/restore-leg-snapshots-from-archive.py.

uv run python -m unittest discover -s scripts/tests

The golden document (fixtures/leg-snapshot-archive.golden.json) is also read by
a Rust test in crates/api/src/data/leg_snapshot.rs, which parses it with
`LegSnapshot::parse` and checks it round-trips: the two sides cannot drift.
Regenerate it with UPDATE_GOLDEN=1.
"""

import contextlib
import datetime as dt
import hashlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from collections import Counter
from pathlib import Path
from types import ModuleType
from typing import override

SCRIPT = (
    Path(__file__).resolve().parent.parent / "restore-leg-snapshots-from-archive.py"
)
GOLDEN = (
    Path(__file__).resolve().parent / "fixtures" / "leg-snapshot-archive.golden.json"
)
RUN_AT = dt.datetime(2026, 11, 21, 12, 0, 0, tzinfo=dt.UTC)


def load_script() -> ModuleType:
    """Import the hyphenated script as a module."""
    spec = importlib.util.spec_from_file_location("restore_leg_snapshots", SCRIPT)
    if spec is None or spec.loader is None:
        raise ImportError(SCRIPT)
    module = importlib.util.module_from_spec(spec)
    sys.modules["restore_leg_snapshots"] = module
    spec.loader.exec_module(module)
    return module


rs = load_script()

TIPLOCS = {
    "PADTON": "PAD",
    "READING": "RDG",
    "DIDCOTP": "DID",
    "SWINDON": "SWI",
    "OXFORD": "OXF",
}


def points() -> list[dict[str, object]]:
    """Return a PAD to SWI timetable (GMT in November)."""
    return [
        {
            "tiploc": "PADTON",
            "kind": "Origin",
            "bookedDeparture": "09:00:00",
            "dayOffset": 0,
            "platform": "3",
        },
        {
            "tiploc": "READING",
            "kind": "Intermediate",
            "bookedArrival": "09:25:00",
            "bookedDeparture": "09:27:00",
            "dayOffset": 0,
            "platform": "7",
            "publicArrival": "09:25:00",
            "publicDeparture": "09:27:00",
        },
        {
            "tiploc": "SWINDON",
            "kind": "Terminate",
            "bookedArrival": "10:05:00",
            "dayOffset": 0,
        },
    ]


def archive_row(uid: str = "C12345", **over: object) -> dict[str, object]:
    """One archived `trains` row, all seventeen columns."""
    row: dict[str, object] = {
        "id": 99,
        "train_uid": uid,
        "train_id": "1A23",
        "service_date": "2026-11-01",
        "origin_crs": "PAD",
        "origin_name": "London Paddington",
        "destination_crs": "SWI",
        "destination_name": "Swindon",
        "scheduled_departure": "2026-11-01T09:00:00+00:00",
        "matched_line_id": "gwr",
        "platform": "3",
        "planned_platform": "3",
        "skipped_stations": [],
        "calling_points": points(),
        "schedule_matched_at": "2026-11-01T06:00:00+00:00",
        "resolved_at": None,
        "created_at": "2026-11-01T05:00:00+00:00",
    }
    row.update(over)
    return row


def dump_line(leg_id: int = 1, **over: object) -> dict[str, object]:
    """One candidate dump line."""
    line: dict[str, object] = {
        "legId": leg_id,
        "publicId": f"leg{leg_id}",
        "subscriptionId": leg_id + 1000,
        "serviceDate": "2026-11-01",
        "subServiceDate": "2026-11-01",
        "legOrigin": "PAD",
        "legDestination": "SWI",
        "pinOrigin": "PAD",
        "pinDestination": "SWI",
        "pinDeparture": "2026-11-01T09:00:00+00:00",
        "recorded": False,
        "delayMinutes": None,
        "cancelled": None,
        "hasSnapshot": False,
    }
    line.update(over)
    return line


def candidate(leg_id: int = 1, **over: object) -> object:
    """Parse one candidate dump line."""
    cand = rs.parse_candidate(dump_line(leg_id, **over))
    assert cand is not None
    return cand


def hits(*rows: dict[str, object], index: int = 0) -> list[object]:
    """Hits for the parsed rows."""
    parsed = [rs.parse_row(r) for r in rows]
    return [rs.Hit(p, index) for p in parsed if p is not None]


RANGE = (dt.date(2026, 10, 1), dt.date(2026, 11, 20))


class Parsing(unittest.TestCase):
    """Reading dump lines, rows and timestamps."""

    def test_instants_in_the_forms_postgres_and_chrono_write(self) -> None:
        want = dt.datetime(2026, 11, 1, 9, 0, tzinfo=dt.UTC)
        for text in (
            "2026-11-01T09:00:00+00:00",
            "2026-11-01T09:00:00Z",
            "2026-11-01 09:00:00+00",
            "2026-11-01T10:00:00+01:00",
        ):
            self.assertEqual(rs.parse_instant(text), want, text)
        self.assertIsNone(rs.parse_instant("2026-11-01T09:00:00"))
        self.assertIsNone(rs.parse_instant(None))

    def test_key_falls_back_to_the_leg_and_needs_a_departure(self) -> None:
        cand = candidate(pinOrigin=None, legOrigin="pad ")
        self.assertEqual(
            rs.candidate_key(cand),
            (dt.date(2026, 11, 1), "PAD", dt.datetime(2026, 11, 1, 9, tzinfo=dt.UTC)),
        )
        self.assertIsNone(rs.candidate_key(candidate(pinDeparture=None)))
        self.assertIsNone(rs.candidate_key(candidate(pinOrigin=None, legOrigin=None)))

    def test_a_line_without_ids_is_dropped(self) -> None:
        self.assertIsNone(rs.parse_candidate({"serviceDate": "2026-11-01"}))
        self.assertIsNone(rs.parse_row({"train_uid": "", "service_date": "2026-11-01"}))

    def test_calling_points_exported_as_text_are_read(self) -> None:
        row = rs.parse_row(archive_row(calling_points=json.dumps(points())))
        self.assertEqual(len(row.calling_points), 3)


class Classify(unittest.TestCase):
    """The match rules."""

    def test_exactly_one_row_is_matched(self) -> None:
        outcome, hit, check = rs.classify(candidate(), hits(archive_row()), {}, RANGE)
        self.assertEqual(outcome, rs.Outcome.MATCHED)
        self.assertEqual(hit.row.train_uid, "C12345")
        self.assertEqual(check, rs.EndCheck.ROW_END)

    def test_two_rows_are_ambiguous_never_guessed(self) -> None:
        found = hits(archive_row("C1"), archive_row("C2"))
        outcome, hit, _ = rs.classify(candidate(), found, {}, RANGE)
        self.assertEqual(outcome, rs.Outcome.AMBIGUOUS)
        self.assertIsNone(hit)

    def test_the_destination_picks_between_two_rows(self) -> None:
        found = hits(archive_row("C1"), archive_row("C2", destination_crs="OXF"))
        outcome, hit, _ = rs.classify(candidate(), found, {}, RANGE)
        self.assertEqual(outcome, rs.Outcome.MATCHED)
        self.assertEqual(hit.row.train_uid, "C1")

    def test_a_destination_the_row_never_reaches_is_a_mismatch(self) -> None:
        outcome, _, _ = rs.classify(
            candidate(pinDestination="BRI"), hits(archive_row()), TIPLOCS, RANGE
        )
        self.assertEqual(outcome, rs.Outcome.MISMATCHED)

    def test_a_later_calling_point_reaches_the_destination_with_the_map(self) -> None:
        cand = candidate(pinDestination="RDG", legDestination="RDG")
        outcome, _, check = rs.classify(cand, hits(archive_row()), TIPLOCS, RANGE)
        self.assertEqual((outcome, check), (rs.Outcome.MATCHED, rs.EndCheck.CALLING))
        without_map, _, _ = rs.classify(cand, hits(archive_row()), {}, RANGE)
        self.assertEqual(without_map, rs.Outcome.MISMATCHED)

    def test_a_calling_point_before_the_origin_does_not_reach_it(self) -> None:
        row = archive_row()
        cand = candidate(pinDestination="PAD", legDestination="PAD")
        hit = rs.Hit(rs.parse_row(row), 2)  # boarded at Reading, PAD is behind
        outcome, _, _ = rs.classify(cand, [hit], TIPLOCS, RANGE)
        self.assertEqual(outcome, rs.Outcome.MISMATCHED)

    def test_no_recorded_destination_matches_and_is_flagged(self) -> None:
        cand = candidate(pinDestination=None, legDestination=None)
        outcome, _, check = rs.classify(cand, hits(archive_row()), {}, RANGE)
        self.assertEqual((outcome, check), (rs.Outcome.MATCHED, rs.EndCheck.UNCHECKED))

    def test_none_is_split_by_the_archive_range(self) -> None:
        old = candidate(serviceDate="2026-09-01", subServiceDate="2026-09-01")
        new = candidate(serviceDate="2026-11-21", subServiceDate="2026-11-21")
        mid = candidate()
        self.assertEqual(rs.classify(old, [], {}, RANGE)[0], rs.Outcome.OLDER)
        self.assertEqual(rs.classify(new, [], {}, RANGE)[0], rs.Outcome.NEWER)
        self.assertEqual(rs.classify(mid, [], {}, RANGE)[0], rs.Outcome.NO_ROW)
        self.assertEqual(rs.classify(mid, [], {}, None)[0], rs.Outcome.NO_ROW)

    def test_snapshotted_no_key_and_date_disagreement(self) -> None:
        self.assertEqual(
            rs.classify(candidate(hasSnapshot=True), hits(archive_row()), {}, RANGE)[0],
            rs.Outcome.SNAPSHOTTED,
        )
        self.assertEqual(
            rs.classify(candidate(pinDeparture=None), [], {}, RANGE)[0],
            rs.Outcome.NO_KEY,
        )
        self.assertEqual(
            rs.classify(
                candidate(subServiceDate="2026-11-02"), hits(archive_row()), {}, RANGE
            )[0],
            rs.Outcome.MISMATCHED,
        )

    def test_unusable_rows_are_counted_not_written(self) -> None:
        empty = hits(archive_row(calling_points=[]))
        self.assertEqual(
            rs.classify(candidate(), empty, {}, RANGE)[0], rs.Outcome.UNUSABLE
        )
        recorded = candidate(recorded=True, delayMinutes=3, cancelled=False)
        self.assertEqual(rs.classify(recorded, empty, {}, RANGE)[0], rs.Outcome.MATCHED)
        nul = hits(archive_row(platform="a\u0000b"))
        self.assertEqual(
            rs.classify(candidate(), nul, {}, RANGE)[0], rs.Outcome.UNUSABLE
        )


class Events(unittest.TestCase):
    """Departures of an archived row, own origin and calling points."""

    def test_calling_points_give_london_instants(self) -> None:
        row = rs.parse_row(
            archive_row(
                service_date="2026-07-01",
                scheduled_departure="2026-07-01T08:00:00+00:00",
            )
        )
        events = list(rs.departure_events(row, TIPLOCS))
        self.assertIn(("PAD", dt.datetime(2026, 7, 1, 8, 0, tzinfo=dt.UTC), 0), events)
        # 09:27 BST is 08:27 UTC; Reading is the second point
        self.assertIn(("RDG", dt.datetime(2026, 7, 1, 8, 27, tzinfo=dt.UTC), 2), events)

    def test_without_the_map_only_the_origin_departs(self) -> None:
        row = rs.parse_row(archive_row())
        self.assertEqual(len(list(rs.departure_events(row, {}))), 1)

    def test_day_offset_moves_the_date(self) -> None:
        row = rs.parse_row(
            archive_row(
                calling_points=[
                    {
                        "tiploc": "READING",
                        "kind": "Intermediate",
                        "bookedDeparture": "00:10:00",
                        "dayOffset": 1,
                    }
                ],
            ),
        )
        events = list(rs.departure_events(row, TIPLOCS))
        self.assertIn(
            ("RDG", dt.datetime(2026, 11, 2, 0, 10, tzinfo=dt.UTC), 1), events
        )


class Document(unittest.TestCase):
    """The version 1 snapshot document."""

    def build(self, **over: object) -> dict[str, object]:
        row = rs.parse_row(archive_row("C12345", skipped_stations=["RDG"], **over))
        cand = candidate(recorded=True, delayMinutes=7, cancelled=False)
        doc: dict[str, object] = rs.build_document(row, cand, RUN_AT)
        return doc

    def test_matches_the_golden_file(self) -> None:
        doc = self.build()
        if os.environ.get("UPDATE_GOLDEN") == "1":
            GOLDEN.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")
        self.assertEqual(doc, json.loads(GOLDEN.read_text()))

    def test_shape(self) -> None:
        doc = self.build()
        self.assertEqual(doc["v"], 1)
        self.assertEqual(doc["source"], "archive")
        self.assertEqual(doc["capturedAt"], "2026-11-21T12:00:00Z")
        self.assertEqual(doc["scheduledDeparture"], "2026-11-01T09:00:00Z")
        self.assertEqual(doc["finalArrivalDelayMinutes"], 7)
        self.assertIs(doc["trainCancelled"], False)
        self.assertNotIn(None, doc.values())

    def test_unknowns_are_absent_and_no_outcome_without_a_recording(self) -> None:
        row = rs.parse_row(
            archive_row(
                train_id=None, platform=None, planned_platform="", matched_line_id=None
            )
        )
        doc = rs.build_document(row, candidate(delayMinutes=5, cancelled=True), RUN_AT)
        for absent in (
            "trainId",
            "platform",
            "plannedPlatform",
            "matchedLineId",
            "skippedStations",
        ):
            self.assertNotIn(absent, doc)
        self.assertNotIn("finalArrivalDelayMinutes", doc)
        self.assertNotIn("trainCancelled", doc)

    def test_chrono_fractions(self) -> None:
        self.assertEqual(
            rs.rfc3339(dt.datetime(2026, 1, 1, tzinfo=dt.UTC)), "2026-01-01T00:00:00Z"
        )
        self.assertEqual(
            rs.rfc3339(dt.datetime(2026, 1, 1, 0, 0, 0, 120000, tzinfo=dt.UTC)),
            "2026-01-01T00:00:00.120Z",
        )
        self.assertEqual(
            rs.rfc3339(dt.datetime(2026, 1, 1, 0, 0, 0, 120001, tzinfo=dt.UTC)),
            "2026-01-01T00:00:00.120001Z",
        )


HOSTILE = [
    "plain",
    "it's",
    "$$",
    "$ds0$ end",
    "$ds0",
    "x$",
    "back\\slash \\' \\u0041",
    "new\nline\ttab",
    "'; DROP TABLE journey_legs; --",
    "unicode ☃ \U0001f682",
    "$ds0$ $ds1$ $ds2$",
]


class SqlText(unittest.TestCase):
    """Quoting and the idempotency predicate."""

    def test_dollar_quote_never_contains_its_own_tag(self) -> None:
        for text in HOSTILE:
            quoted = rs.dollar_quote(text)
            tag = quoted[: quoted.index("$", 1) + 1]
            self.assertTrue(quoted.startswith(tag) and quoted.endswith(tag), text)
            body = quoted[len(tag) : -len(tag)]
            self.assertEqual(body, text)
            self.assertNotIn(tag, body)

    def test_nul_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            rs.dollar_quote("a\x00b")

    def test_the_statement_only_applies_while_nothing_changed(self) -> None:
        row = rs.parse_row(archive_row())
        sql = rs.statement(candidate(7), rs.build_document(row, candidate(7), RUN_AT))
        for clause in (
            "jl.id = 7",
            "jl.final_snapshot IS NULL",
            "jl.train_subscription_id = 1007",
            "ts.id = 1007",
            "ts.trains_id IS NULL",
            "jl.final_recorded_at IS NULL",
            "jl.kind = 'train'",
        ):
            self.assertIn(clause, sql)
        recorded = rs.statement(
            candidate(7, recorded=True),
            rs.build_document(row, candidate(7, recorded=True), RUN_AT),
        )
        self.assertIn("jl.final_recorded_at IS NOT NULL", recorded)
        self.assertIn("+ interval '1 second'", recorded)

    def test_hostile_row_text_stays_inside_its_literal(self) -> None:
        for text in HOSTILE:
            row = rs.parse_row(archive_row(train_uid=text, platform=text))
            sql = rs.statement(candidate(), rs.build_document(row, candidate(), RUN_AT))
            self.assertTrue(sql.startswith("UPDATE journey_legs"))
            self.assertTrue(sql.endswith(";"))

    def test_batches_are_one_transaction_each(self) -> None:
        row = rs.parse_row(archive_row())
        items = [
            (candidate(i), rs.build_document(row, candidate(i), RUN_AT))
            for i in range(1, 451)
        ]
        out = rs.batches(items, 200)
        self.assertEqual([t.count("UPDATE journey_legs") for t in out], [200, 200, 50])
        for text in out:
            lines = text.splitlines()
            self.assertEqual(lines[0], "BEGIN;")
            self.assertEqual(lines[-1], "COMMIT;")
            self.assertEqual(text.count("BEGIN;"), 1)
        self.assertEqual(rs.batches([], 200), [])

    def test_verify_and_rollback_are_scoped_to_the_ids(self) -> None:
        verify = rs.verify_sql([3, 1, 2])
        self.assertIn("ARRAY[1,2,3]::bigint[]", verify)
        self.assertTrue(verify.lstrip().startswith("--"))
        self.assertNotIn("UPDATE", verify)
        back = rs.rollback_sql([1, 2], RUN_AT)
        self.assertIn("final_snapshot->>'source' = 'archive'", back)
        self.assertIn("2026-11-21T12:00:00Z", back)


class Files(unittest.TestCase):
    """Reading a local archive mirror and writing the output directory."""

    @override
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def write_archive(self, zst: bool = False) -> Path:
        arc = self.root / "archive"
        day = arc / "service_date=2026-11-01"
        day.mkdir(parents=True)
        other = arc / "service_date=2026-10-15"
        other.mkdir(parents=True)
        lines = [
            archive_row("C1"),
            archive_row("C9", destination_crs="OXF"),
            archive_row("C1"),
        ]
        text = "\n".join(json.dumps(r) for r in lines) + "\nnot json\n"
        if zst:
            plain = day / "part-1.jsonl"
            plain.write_text(text)
            subprocess.run(["zstd", "-q", "--rm", str(plain)], check=True)
        else:
            (day / "part-1.jsonl").write_text(text)
        (other / "part-2.jsonl").write_text(
            json.dumps(archive_row("C5", service_date="2026-10-15")) + "\n"
        )
        return arc

    def run_cli(self, *argv: str) -> str:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(rs.main(list(argv)), 0)
        return out.getvalue()

    def write_candidates(self) -> Path:
        path = self.root / "cands.jsonl"
        lines = [
            dump_line(1),
            dump_line(2, pinDestination="OXF", legDestination="OXF"),
            dump_line(3, hasSnapshot=True),
            dump_line(4, serviceDate="2026-09-01", subServiceDate="2026-09-01"),
            dump_line(5, pinDeparture=None),
            dump_line(6, pinDeparture="2026-11-01T09:01:00+00:00"),
        ]
        path.write_text("\n".join(json.dumps(x) for x in lines) + "\n")
        return path

    def test_scan_dedupes_a_row_written_twice_and_counts_bad_lines(self) -> None:
        unreadable = [0]
        arc = rs.LocalArchive(self.write_archive(), unreadable)
        listing = arc.listing()
        self.assertEqual(sorted(listing), [dt.date(2026, 10, 15), dt.date(2026, 11, 1)])
        key = (dt.date(2026, 11, 1), "PAD", dt.datetime(2026, 11, 1, 9, tzinfo=dt.UTC))
        scan = rs.scan_archive(arc, listing, {key}, {})
        self.assertEqual(sorted(uid for uid, _ in scan.hits[key]), ["C1", "C9"])
        self.assertEqual((scan.dates_read, scan.rows_scanned, unreadable[0]), (1, 3, 1))

    def test_dry_run_prints_counts_only(self) -> None:
        arc = self.write_archive()
        out = self.run_cli(
            "restore",
            "--candidates",
            str(self.write_candidates()),
            "--archive-dir",
            str(arc),
        )
        self.assertIn("candidates: 6", out)
        self.assertIn(
            "  matched: 2", out
        )  # legs 1 (SWI) and 2 (OXF) each reach one row
        self.assertIn("  already snapshotted: 1", out)
        self.assertIn("older than the earliest archived service date: 1", out)
        self.assertIn("no recorded origin or departure to match on: 1", out)
        self.assertIn("within the archive range but no row: 1", out)
        self.assertIn("total accounted: 6", out)
        self.assertIn("dry run: nothing written", out)
        for secret in ("leg1", "C1", "C9", "1007"):
            self.assertNotIn(secret, out)

    def test_verbose_adds_only_opaque_public_ids(self) -> None:
        arc = self.write_archive()
        out = self.run_cli(
            "restore",
            "--candidates",
            str(self.write_candidates()),
            "--archive-dir",
            str(arc),
            "--verbose",
        )
        self.assertIn("# matched (2): leg1 leg2", out)
        self.assertNotIn("C12345", out)

    def test_emit_writes_checksummed_private_files(self) -> None:
        arc = self.write_archive(zst=shutil.which("zstd") is not None)
        dest = self.root / "out"
        out = self.run_cli(
            "restore",
            "--candidates",
            str(self.write_candidates()),
            "--archive-dir",
            str(arc),
            "--emit-sql",
            str(dest),
            "--batch-size",
            "1",
        )
        self.assertIn("for 2 legs", out)
        manifest = json.loads((dest / "manifest.json").read_text())
        names = [f["name"] for f in manifest["files"]]
        self.assertEqual(
            names, ["batch-0001.sql", "batch-0002.sql", "verify.sql", "rollback.sql"]
        )
        for entry in manifest["files"]:
            data = (dest / entry["name"]).read_bytes()
            self.assertEqual(hashlib.sha256(data).hexdigest(), entry["sha256"])
            self.assertEqual(len(data), entry["bytes"])
            self.assertEqual((dest / entry["name"]).stat().st_mode & 0o777, 0o600)
        self.assertEqual(manifest["counts"]["matched"], 2)
        self.assertEqual(manifest["legsToApply"], 2)
        self.assertNotIn("C1", json.dumps(manifest))

    def test_dump_sql_is_read_only(self) -> None:
        out = self.run_cli("dump-sql")
        self.assertIn("FROM journey_legs jl", out)
        self.assertIn("tiploc_crs", out)
        for word in ("INSERT", "UPDATE", "DELETE", "DROP"):
            self.assertNotIn(word, out)

    def test_counts_add_up(self) -> None:
        counts: Counter[object] = Counter({rs.Outcome.MATCHED: 2})
        self.assertEqual(sum(counts.values()), 2)


@unittest.skipUnless(
    shutil.which("initdb") and shutil.which("pg_ctl") and shutil.which("psql"),
    "needs a local PostgreSQL",
)
class AgainstPostgres(unittest.TestCase):
    """Apply the emitted SQL to a throwaway cluster with the two tables' shape."""

    data: str
    sock: str

    SCHEMA = """
    CREATE TABLE train_subscriptions (id bigint PRIMARY KEY, trains_id bigint);
    CREATE TABLE journey_legs (
        id bigint PRIMARY KEY, kind text NOT NULL DEFAULT 'train',
        train_subscription_id bigint REFERENCES train_subscriptions(id),
        final_recorded_at timestamptz, final_train_uid text,
        final_snapshot jsonb, final_snapshot_at timestamptz,
        CHECK (final_snapshot IS NULL OR jsonb_typeof(final_snapshot) = 'object'));
    INSERT INTO train_subscriptions VALUES
        (1001, NULL), (1002, NULL), (1003, 55), (1004, NULL);
    INSERT INTO journey_legs (id, train_subscription_id, final_recorded_at) VALUES
        (1, 1001, NULL), (2, 1002, now() - interval '1 day'),
        (3, 1003, NULL), (4, 1004, NULL);
    """

    @override
    @classmethod
    def setUpClass(cls) -> None:
        cls.data = tempfile.mkdtemp(prefix="pgdata")
        # a short path: unix socket paths are limited to about 100 bytes
        cls.sock = tempfile.mkdtemp(prefix="pg", dir="/tmp")
        cls.addClassCleanup(shutil.rmtree, cls.data, ignore_errors=True)
        cls.addClassCleanup(shutil.rmtree, cls.sock, ignore_errors=True)
        subprocess.run(
            ["initdb", "-D", cls.data, "-A", "trust", "-U", "t"],
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
        cls.psql("postgres", "CREATE DATABASE t")
        cls.psql("t", cls.SCHEMA)

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

    def test_apply_twice_is_idempotent_and_guards_hold(self) -> None:
        hostile = "C'1$ds0$\\"
        row = rs.parse_row(archive_row(train_uid=hostile, platform="$$ \\n 'x"))
        items = [
            (candidate(1), rs.build_document(row, candidate(1), RUN_AT)),
            (
                candidate(2, recorded=True, delayMinutes=4, cancelled=False),
                rs.build_document(
                    row,
                    candidate(2, recorded=True, delayMinutes=4, cancelled=False),
                    RUN_AT,
                ),
            ),
            # leg 3: its subscription has a train again; leg 4: its recorded
            # flag disagrees (the leg is not recorded)
            (candidate(3), rs.build_document(row, candidate(3), RUN_AT)),
            (
                candidate(4, recorded=True, delayMinutes=1, cancelled=False),
                rs.build_document(
                    row,
                    candidate(4, recorded=True, delayMinutes=1, cancelled=False),
                    RUN_AT,
                ),
            ),
        ]
        (text,) = rs.batches(items, 200)
        self.psql("t", text)
        rows = self.psql(
            "t",
            "SELECT id, final_train_uid, final_snapshot->>'source', "
            "final_snapshot_at > coalesce(final_recorded_at, '-infinity') "
            "FROM journey_legs ORDER BY id",
        ).splitlines()
        self.assertEqual(rows[0], f"1|{hostile}|archive|t")
        self.assertEqual(rows[1], f"2|{hostile}|archive|t")
        self.assertEqual(rows[2], "3|||")  # a train came back: untouched
        self.assertEqual(rows[3], "4|||")  # outcome flag disagreed: untouched
        stored = json.loads(
            self.psql("t", "SELECT final_snapshot FROM journey_legs WHERE id = 2")
        )
        self.assertEqual(stored, items[1][1])
        before = self.psql(
            "t", "SELECT id, final_snapshot_at FROM journey_legs ORDER BY id"
        )
        self.psql("t", text)  # again: no row changes
        self.assertEqual(
            before,
            self.psql(
                "t", "SELECT id, final_snapshot_at FROM journey_legs ORDER BY id"
            ),
        )
        verify = rs.verify_sql([1, 2])
        self.assertEqual(self.psql("t", verify).strip(), "2|2|0|0")
        self.psql("t", rs.rollback_sql([1, 2], RUN_AT))
        self.assertEqual(
            self.psql(
                "t",
                "SELECT count(*) FROM journey_legs WHERE final_snapshot IS NOT NULL",
            ).strip(),
            "0",
        )


if __name__ == "__main__":
    unittest.main()
