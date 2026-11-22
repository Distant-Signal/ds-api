#!/usr/bin/env python3
# ruff: noqa: T201, S608  # a CLI whose report is stdout; SQL built from ints and dollar-quoted literals
"""Recover pruned trains, with their original ids, from a point-in-time restore.

Tracking a train is a one-leg journey. The aggregator now pins a `trains` row
while a leg is bound to it, but legs whose train was pruned before that lost
the live row (their timetable was restored onto the leg as a snapshot, see
docs/leg-snapshot-restore.md). pgBackRest backups still hold some of the
original rows. This script reads them from a scratch Postgres restored to a
point in time and writes SQL that puts the full live rows back:

  trains, train_movement_events, train_current_state, train_reasons and
  train_service_labels, all with their ORIGINAL ids, and
  train_subscriptions.trains_id relinked

so the leg renders live again and the pin keeps the train. It NEVER writes to
any database: the scratch is read with `default_transaction_read_only`, and
the SQL it emits is for someone else to apply. See docs/pitr-train-extraction.md.

Input: targets.jsonl, one object per leg::

  {"subscriptionId": 1, "trainUid": "C12345",
   "serviceDate": "2026-10-10", "target": "T1"}

`--target` selects which lines a run reads (T1 and T2 are two scratch
instances restored to different times); other targets' lines are ignored.

Subcommands::

  extract    read the selected trains from the scratch Postgres into a JSONL
             file (--out), optionally emitting the SQL (--emit-sql DIR)
  plan       counts only, from an extract file
  preflight  write the read-only live-database SQL (counts only) from an
             extract file
  emit       write the SQL directory from an extract file

Matching: a target's train is the `trains` row with that (train_uid,
service_date). Exactly one: extracted. None: counted `no_match`, skipped.
Several (the unique constraint makes this impossible, but it is checked):
counted `ambiguous`, skipped. A train whose rows carry a NUL byte (it cannot
be written back faithfully), whose column set is not the one this script
knows, or whose children do not point at it is counted `rejected`, skipped.
It never guesses.

Guards in the emitted SQL (see `train_statement`): a train is inserted only if
no live row has its id, none has the same (train_uid, service_date) or
(train_id, service_date) (a re-tracked train made a fresh row), and none of its
event or current-state ids is taken. Children are inserted in the same
statement as, and only when, the train row is. Otherwise the train is a no-op
and the relink does not apply. The one UPDATE of an existing row is the relink
`SET trains_id` where it is still NULL, on the same service date, and only
where the live trains row with that id, uid and date exists. Counts only are
printed; --verbose adds the opaque subscription ids per outcome.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import datetime as dt
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Iterable, Iterator, Sequence

SCRIPT_VERSION = 1
DEFAULT_BATCH = 25
TARGETS = ("T1", "T2")
DATE_RE = re.compile(r"\d{4}-\d{2}-\d{2}")

# The columns of each table as of ds-core migration 20261120130200 (live and
# the restored scratch are identical: the live schema differs only by
# journey_legs columns). Rows are re-inserted with these explicit lists, and
# an extracted row whose key set differs is rejected; the emitted SQL checks
# the live column sets too, so a changed schema fails closed.
TRAINS_COLS = (
    "id",
    "train_uid",
    "service_date",
    "origin_crs",
    "scheduled_departure",
    "destination_crs",
    "calling_points",
    "matched_line_id",
    "schedule_matched_at",
    "train_id",
    "resolved_at",
    "created_at",
    "skipped_stations",
    "platform",
    "planned_platform",
    "origin_name",
    "destination_name",
)
EVENTS_COLS = (
    "id",
    "dedup_key",
    "msg_type",
    "event_type",
    "loc_stanox",
    "loc_crs",
    "planned_timestamp",
    "actual_timestamp",
    "variation_status",
    "raw_body",
    "received_at",
    "trains_id",
    "gbtt_timestamp",
    "platform",
)
STATE_COLS = (
    "status",
    "last_reported_location",
    "last_event_type",
    "delay_minutes",
    "next_calling_point",
    "eta_next",
    "eta_source",
    "updated_at",
    "id",
    "trains_id",
    "event_time",
)
REASONS_COLS = (
    "trains_id",
    "msg_type",
    "reason_code",
    "canx_type",
    "loc_stanox",
    "event_at",
    "received_at",
)
LABELS_COLS = ("trains_id", "service_class", "toc_id")
TABLES = {
    "trains": TRAINS_COLS,
    "train_movement_events": EVENTS_COLS,
    "train_current_state": STATE_COLS,
    "train_reasons": REASONS_COLS,
    "train_service_labels": LABELS_COLS,
}

Row = dict[str, object]


@dataclasses.dataclass(frozen=True)
class Target:
    """One leg's request."""

    subscription_id: int
    train_uid: str
    service_date: dt.date
    target: str


@dataclasses.dataclass
class Bundle:
    """One train and its children, as read from the scratch."""

    train: Row
    events: list[Row]
    state: Row | None
    reasons: list[Row]
    label: Row | None
    subs: list[int]

    @property
    def train_id(self) -> int:
        """The original `trains.id`."""
        return int(str(self.train["id"]))

    @property
    def uid(self) -> str:
        """The train uid."""
        return str(self.train["train_uid"])

    @property
    def date(self) -> str:
        """The service date, ISO."""
        return str(self.train["service_date"])

    def doc(self) -> Row:
        """Return the whole bundle as one JSON document (what the SQL embeds)."""
        return {
            "train": self.train,
            "events": self.events,
            "state": self.state,
            "reasons": self.reasons,
            "label": self.label,
            "subs": self.subs,
        }

    def to_json(self) -> Row:
        """Return the extract-file form."""
        return {"kind": "bundle", **self.doc()}


# --- input -----------------------------------------------------------------


def parse_target(obj: object) -> Target:
    """Parse and validate one targets.jsonl object; raise ValueError."""
    if not isinstance(obj, dict):
        message = "target line is not an object"
        raise ValueError(message)  # noqa: TRY004
    sub = obj.get("subscriptionId")
    uid = obj.get("trainUid")
    day = obj.get("serviceDate")
    which = obj.get("target")
    if not isinstance(sub, int) or isinstance(sub, bool) or sub < 1:
        message = "subscriptionId must be a positive integer"
        raise ValueError(message)
    if not isinstance(uid, str) or not uid or "\x00" in uid:
        message = "trainUid must be a non-empty string without NUL"
        raise ValueError(message)
    if not isinstance(day, str) or not DATE_RE.fullmatch(day):
        message = "serviceDate must be YYYY-MM-DD"
        raise ValueError(message)
    if which not in TARGETS:
        message = "target must be T1 or T2"
        raise ValueError(message)
    return Target(sub, uid, dt.date.fromisoformat(day), which)


def read_targets(path: Path, target: str) -> tuple[list[Target], int]:
    """Return the targets of `target`, and how many lines were the other's."""
    chosen: list[Target] = []
    other = 0
    with path.open(encoding="utf-8") as handle:
        for number, line in enumerate(handle, start=1):
            if not line.strip():
                continue
            try:
                item = parse_target(json.loads(line))
            except ValueError as exc:
                message = f"{path.name} line {number}: {exc}"
                raise SystemExit(message) from None
            if item.target == target:
                chosen.append(item)
            else:
                other += 1
    return chosen, other


# --- scratch access --------------------------------------------------------


def dollar_quote(text: str) -> str:
    """`text` as a dollar-quoted literal whose tag does not occur in it.

    Dollar quotes need no escaping and, unlike '...', do not depend on
    standard_conforming_strings. NUL cannot appear in a PostgreSQL text.
    """
    if "\x00" in text:
        message = "NUL in a SQL text"
        raise ValueError(message)
    n = 0
    while True:
        tag = f"$ds{n}$"
        if tag not in text and not text.endswith(f"$ds{n}"):
            return f"{tag}{text}{tag}"
        n += 1


def has_nul(value: object) -> bool:
    """Whether any string (or key) in a JSON value holds a NUL byte."""
    if isinstance(value, str):
        return "\x00" in value
    if isinstance(value, list):
        return any(has_nul(item) for item in value)
    if isinstance(value, dict):
        return any(has_nul(k) or has_nul(v) for k, v in value.items())
    return False


# Backups from before train_service_labels existed (the 2026-11-09 one) lack
# the table, and a query naming a missing table fails at parse time. The
# scratch script asks `to_regclass` first and `\gset`s the label subquery into a
# psql variable: the real subquery when the table exists (identical to the old
# query), `NULL::jsonb` (no label for any train) when it does not.
LABEL_VAR = "label_expr"
LABEL_EXPR = f":{LABEL_VAR}"
LABEL_SUBQUERY = (
    "(SELECT to_jsonb(l) FROM train_service_labels l WHERE l.trains_id = t.id)"
)
LABEL_PROBE = (
    "SELECT CASE WHEN to_regclass('public.train_service_labels') IS NOT NULL "
    f"THEN {dollar_quote(LABEL_SUBQUERY)} ELSE 'NULL::jsonb' END AS {LABEL_VAR} "
    "\\gset"
)


def bundle_sql(index: int, uid: str, day: dt.date) -> str:
    """Return the read-only query for one train: a JSON line per matching row.

    jsonb `to_jsonb` carries every column; timestamptz come out with an
    offset (the session is UTC) and microseconds, so the instant is exact.
    """
    return (
        "SELECT jsonb_build_object("
        f"'idx', {index}, "
        "'train', to_jsonb(t), "
        "'events', COALESCE((SELECT jsonb_agg(to_jsonb(e) ORDER BY e.id) "
        "FROM train_movement_events e WHERE e.trains_id = t.id), '[]'::jsonb), "
        "'state', (SELECT to_jsonb(s) FROM train_current_state s "
        "WHERE s.trains_id = t.id), "
        "'reasons', COALESCE((SELECT jsonb_agg(to_jsonb(r) ORDER BY r.msg_type) "
        "FROM train_reasons r WHERE r.trains_id = t.id), '[]'::jsonb), "
        f"'label', {LABEL_EXPR})::text "
        f"FROM trains t WHERE t.train_uid = {dollar_quote(uid)} "
        f"AND t.service_date = '{day.isoformat()}'::date;"
    )


def scratch_script(keys: Sequence[tuple[str, dt.date]]) -> str:
    """Return the whole read-only session for a set of (uid, date) keys."""
    queries = "\n".join(
        bundle_sql(index, uid, day) for index, (uid, day) in enumerate(keys)
    )
    return (
        "SET default_transaction_read_only = on;\n"
        "SET TimeZone = 'UTC';\n"
        "BEGIN READ ONLY;\n"
        f"{LABEL_PROBE}\n"
        f"{queries}\n"
        "ROLLBACK;\n"
    )


def run_psql(psql: str, script: str, *, verbose: bool) -> str:
    """Run `script` through the owner's psql command prefix, on stdin."""
    cmd = [*shlex.split(psql), "-v", "ON_ERROR_STOP=1"]
    done = subprocess.run(  # noqa: S603  # the owner's own command
        cmd,
        input=script,
        capture_output=True,
        text=True,
        check=False,
    )
    if done.returncode != 0:
        detail = (
            f": {done.stderr.strip()}" if verbose else " (--verbose shows psql's error)"
        )
        message = f"psql exited {done.returncode}{detail}"
        raise SystemExit(message)
    return done.stdout


def parse_lines(output: str) -> Iterator[Row]:
    """Yield the JSON object lines of psql output (command tags are skipped)."""
    for line in output.splitlines():
        if line.startswith("{"):
            obj = json.loads(line)
            if isinstance(obj, dict):
                yield obj


class Rejected(ValueError):  # noqa: N818  # a skip reason, not an error
    """A train that cannot be carried over faithfully."""


def shape_ok(row: Row, cols: Sequence[str]) -> bool:
    """Whether a row's keys are exactly the table's columns."""
    return set(row) == set(cols)


def as_rows(value: object) -> list[Row]:
    """Return a JSON array of objects, else raise Rejected."""
    if not isinstance(value, list) or not all(isinstance(v, dict) for v in value):
        message = "children are not an array of objects"
        raise Rejected(message)
    return list(value)


def optional_row(value: object, cols: Sequence[str]) -> Row | None:
    """Return a row of `cols` or None; raise Rejected for anything else."""
    if value is None:
        return None
    if not isinstance(value, dict) or not shape_ok(value, cols):
        message = "a child table's columns differ"
        raise Rejected(message)
    return value


def build_bundle(obj: Row, subs: list[int]) -> Bundle:
    """Validate one scratch line into a Bundle, else raise Rejected."""
    if has_nul(obj):
        message = "NUL byte"
        raise Rejected(message)
    train = obj.get("train")
    if not isinstance(train, dict) or not shape_ok(train, TRAINS_COLS):
        message = "trains columns differ"
        raise Rejected(message)
    events = as_rows(obj.get("events"))
    reasons = as_rows(obj.get("reasons"))
    state = optional_row(obj.get("state"), STATE_COLS)
    label = optional_row(obj.get("label"), LABELS_COLS)
    if not all(shape_ok(e, EVENTS_COLS) for e in events) or not all(
        shape_ok(r, REASONS_COLS) for r in reasons
    ):
        message = "a child table's columns differ"
        raise Rejected(message)
    tid = train["id"]
    children = [*events, *reasons, *(c for c in (state, label) if c is not None)]
    if not isinstance(tid, int) or any(c["trains_id"] != tid for c in children):
        message = "children do not point at the train"
        raise Rejected(message)
    ids = [e["id"] for e in events] + ([state["id"]] if state is not None else [])
    if not all(isinstance(i, int) for i in ids):
        message = "non-integer child id"
        raise Rejected(message)
    return Bundle(train, events, state, reasons, label, sorted(subs))


@dataclasses.dataclass
class Extraction:
    """The result of reading a target set."""

    target: str
    bundles: list[Bundle]
    counts: Counter[str]
    outcome_subs: dict[str, list[int]]


def extract_target(
    targets: Sequence[Target], psql: str, *, verbose: bool, target: str
) -> Extraction:
    """Read each distinct (uid, date) of `targets` from the scratch."""
    grouped: dict[tuple[str, dt.date], list[int]] = {}
    for item in targets:
        grouped.setdefault((item.train_uid, item.service_date), []).append(
            item.subscription_id
        )
    keys = list(grouped)
    counts: Counter[str] = Counter(targets=len(targets), distinct_trains=len(keys))
    outcome_subs: dict[str, list[int]] = {}
    found: dict[int, list[Row]] = {}
    if keys:
        output = run_psql(psql, scratch_script(keys), verbose=verbose)
        for obj in parse_lines(output):
            index = obj.get("idx")
            if isinstance(index, int):
                found.setdefault(index, []).append(obj)
    bundles: list[Bundle] = []
    for index, key in enumerate(keys):
        subs = grouped[key]
        rows = found.get(index, [])
        outcome = "extracted"
        if not rows:
            outcome = "no_match"
        elif len(rows) > 1:
            outcome = "ambiguous"
        else:
            try:
                bundles.append(build_bundle(rows[0], subs))
            except Rejected:
                outcome = "rejected"
        counts[outcome] += 1
        outcome_subs.setdefault(outcome, []).extend(subs)
    return Extraction(target, bundles, counts, outcome_subs)


# --- extract file ----------------------------------------------------------


def write_private(path: Path, data: bytes) -> None:
    """Write `path` readable by the owner only (the files carry train data)."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as handle:
        handle.write(data)


def dumps(value: object, *, indent: int | None = None) -> str:
    """Stable ASCII JSON (so nothing depends on the output encoding)."""
    if indent is None:
        return json.dumps(value, separators=(",", ":"), sort_keys=True)
    return json.dumps(value, indent=indent, sort_keys=True)


def write_extract(path: Path, ex: Extraction, run_at: dt.datetime) -> None:
    """Write the extract file: a header line, then one bundle per line."""
    header = {
        "kind": "header",
        "scriptVersion": SCRIPT_VERSION,
        "target": ex.target,
        "extractedAt": rfc3339(run_at),
        "counts": dict(ex.counts),
    }
    lines = [dumps(header), *(dumps(b.to_json()) for b in ex.bundles)]
    write_private(path, ("\n".join(lines) + "\n").encode())


def read_extract(path: Path) -> tuple[Row, list[Bundle]]:
    """Read an extract file back (re-validating each bundle)."""
    header: Row = {}
    bundles: list[Bundle] = []
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            obj = json.loads(line)
            if obj.get("kind") == "header":
                header = obj
                continue
            subs = [int(s) for s in obj["subs"]]
            try:
                bundles.append(build_bundle(obj, subs))
            except Rejected as exc:
                message = f"{path.name}: bad bundle: {exc}"
                raise SystemExit(message) from None
    if not header:
        message = f"{path.name}: no header line"
        raise SystemExit(message)
    return header, bundles


def rfc3339(value: dt.datetime) -> str:
    """UTC, whole seconds."""
    return value.astimezone(dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- SQL -------------------------------------------------------------------


def int_array(ids: Iterable[int]) -> str:
    """Return a SQL bigint array literal (ints only)."""
    return "ARRAY[" + ",".join(str(int(i)) for i in sorted(ids)) + "]::bigint[]"


def col_list(cols: Sequence[str], prefix: str = "") -> str:
    """Comma-joined column names."""
    return ", ".join(f"{prefix}{c}" for c in cols)


def shape_check(tops: tuple[int, int, int]) -> str:
    """Return a DO block that aborts unless the live schema and ids fit.

    The live column sets must equal the ones the rows were written with, and
    the live `max(id)` of EACH table (trains, train_movement_events,
    train_current_state, each on its own sequence) must be above the highest id
    restored into that same table, so the sequences cannot hand a restored id
    out later (no setval is needed). Never compare one table's max with another
    table's ids: event ids are far above train ids.
    """
    trains_top, events_top, state_top = tops
    clauses = []
    for table, cols in TABLES.items():
        names = ",".join(sorted(cols))
        clauses.append(
            "(SELECT string_agg(column_name::text, ',' ORDER BY column_name) "
            "FROM information_schema.columns WHERE table_schema = 'public' "
            f"AND table_name = '{table}') IS DISTINCT FROM '{names}'"
        )
    schema_bad = " OR ".join(clauses)
    return (
        "DO $guard$ BEGIN\n"
        f"  IF {schema_bad} THEN\n"
        "    RAISE EXCEPTION 'schema differs from the extracted columns';\n"
        "  END IF;\n"
        f"  IF COALESCE((SELECT max(id) FROM trains), 0) <= {trains_top} THEN\n"
        "    RAISE EXCEPTION 'trains ids are not above the restored trains ids';\n"
        "  END IF;\n"
        "  IF COALESCE((SELECT max(id) FROM train_movement_events), 0) "
        f"<= {events_top} THEN\n"
        "    RAISE EXCEPTION 'event ids are not above the restored event ids';\n"
        "  END IF;\n"
        "  IF COALESCE((SELECT max(id) FROM train_current_state), 0) "
        f"<= {state_top} THEN\n"
        "    RAISE EXCEPTION 'state ids are not above the restored state ids';\n"
        "  END IF;\n"
        "END $guard$;"
    )


def train_statement(bundle: Bundle) -> str:
    """Return the guarded insert of a train and its children, one statement.

    The train is inserted only when no live row has its id, its
    (train_uid, service_date) or its (train_id, service_date), and no event
    or current-state id is taken. Children come from the CTE of the train row
    that was inserted, so they follow it or are all skipped.
    """
    doc = dollar_quote(dumps(bundle.doc()))
    event_ids = int_array(int(str(e["id"])) for e in bundle.events)
    state_clause = ""
    if bundle.state is not None:
        sid = int(str(bundle.state["id"]))
        state_clause = (
            " AND NOT EXISTS (SELECT 1 FROM train_current_state x "
            f"WHERE x.id = {sid} OR x.trains_id = r.id)"
        )
    ctes = [
        f"d AS (SELECT {doc}::jsonb AS j)",
        (
            f"t AS (INSERT INTO trains ({col_list(TRAINS_COLS)}) "
            f"SELECT {col_list(TRAINS_COLS, 'r.')} "
            "FROM d, jsonb_populate_record(NULL::trains, d.j->'train') AS r "
            "WHERE NOT EXISTS (SELECT 1 FROM trains x WHERE x.id = r.id "
            "OR (x.train_uid = r.train_uid AND x.service_date = r.service_date) "
            "OR (r.train_id IS NOT NULL AND x.train_id = r.train_id "
            "AND x.service_date = r.service_date)) "
            "AND NOT EXISTS (SELECT 1 FROM train_movement_events x "
            f"WHERE x.id = ANY({event_ids}))"
            f"{state_clause} RETURNING id)"
        ),
    ]
    if bundle.events:
        ctes.append(
            f"ev AS (INSERT INTO train_movement_events ({col_list(EVENTS_COLS)}) "
            f"SELECT {col_list(EVENTS_COLS, 'r.')} FROM t, d, "
            "jsonb_populate_recordset(NULL::train_movement_events, d.j->'events') "
            "AS r ON CONFLICT DO NOTHING RETURNING 1)"
        )
    if bundle.state is not None:
        ctes.append(
            f"st AS (INSERT INTO train_current_state ({col_list(STATE_COLS)}) "
            f"SELECT {col_list(STATE_COLS, 'r.')} FROM t, d, "
            "jsonb_populate_record(NULL::train_current_state, d.j->'state') "
            "AS r ON CONFLICT DO NOTHING RETURNING 1)"
        )
    if bundle.reasons:
        ctes.append(
            f"re AS (INSERT INTO train_reasons ({col_list(REASONS_COLS)}) "
            f"SELECT {col_list(REASONS_COLS, 'r.')} FROM t, d, "
            "jsonb_populate_recordset(NULL::train_reasons, d.j->'reasons') "
            "AS r ON CONFLICT DO NOTHING RETURNING 1)"
        )
    if bundle.label is not None:
        ctes.append(
            f"lb AS (INSERT INTO train_service_labels ({col_list(LABELS_COLS)}) "
            f"SELECT {col_list(LABELS_COLS, 'r.')} FROM t, d, "
            "jsonb_populate_record(NULL::train_service_labels, d.j->'label') "
            "AS r ON CONFLICT DO NOTHING RETURNING 1)"
        )
    return "WITH " + ",\n  ".join(ctes) + "\nSELECT count(*) FROM t;"


def relink_statements(bundle: Bundle) -> list[str]:
    """One guarded relink per subscription.

    Only while the subscription still has no train, is on the same service
    date, and the trains row with this id, uid and date exists (inserted by
    this run, by an earlier run, or never pruned). A live row for the same
    (uid, date) under a different id fails the EXISTS: no relink.
    """
    uid = dollar_quote(bundle.uid)
    return [
        f"UPDATE train_subscriptions SET trains_id = {bundle.train_id} "
        f"WHERE id = {sub} AND trains_id IS NULL "
        f"AND service_date = '{bundle.date}'::date "
        f"AND EXISTS (SELECT 1 FROM trains x WHERE x.id = {bundle.train_id} "
        f"AND x.train_uid = {uid} AND x.service_date = '{bundle.date}'::date);"
        for sub in bundle.subs
    ]


def max_ids(bundles: Sequence[Bundle]) -> tuple[int, int, int]:
    """Return the highest restored trains, events and current-state ids."""
    return (
        max((b.train_id for b in bundles), default=0),
        max((int(str(e["id"])) for b in bundles for e in b.events), default=0),
        max(
            (int(str(b.state["id"])) for b in bundles if b.state is not None),
            default=0,
        ),
    )


def batches(bundles: Sequence[Bundle], size: int) -> list[str]:
    """One transaction per `size` trains."""
    tops = max_ids(bundles)
    out = []
    for start in range(0, len(bundles), size):
        lines = [
            "BEGIN;",
            "SET LOCAL lock_timeout = '5s';",
            "SET LOCAL statement_timeout = '120s';",
            shape_check(tops),
        ]
        for bundle in bundles[start : start + size]:
            lines.append(train_statement(bundle))
            lines.extend(relink_statements(bundle))
        lines.append("COMMIT;")
        out.append("\n".join(lines) + "\n")
    return out


def docs_literal(bundles: Sequence[Bundle]) -> str:
    """Return the bundles as a dollar-quoted jsonb array."""
    return f"{dollar_quote(dumps([b.doc() for b in bundles]))}::jsonb"


def verify_sql(bundles: Sequence[Bundle]) -> str:
    """Return a read-only check that live rows read back identical.

    Row for row: `to_jsonb(live row) = extracted row`, each looked up by its
    primary key first (an unkeyed comparison scans the whole table). Counts only. For a
    run where every train was inserted, each *_identical equals its
    *_expected; a skipped train (see preflight.sql) shows as a shortfall.
    """
    docs = docs_literal(bundles)
    return (
        "-- Read-only. Each *_identical should equal its *_expected, except for\n"
        "-- trains preflight.sql reported as skipped.\n"
        "SET TimeZone = 'UTC';\n"
        f"WITH d AS (SELECT j FROM jsonb_array_elements({docs}) AS j)\n"
        "SELECT\n"
        "  count(*) AS trains_expected,\n"
        "  count(*) FILTER (WHERE EXISTS (SELECT 1 FROM trains x\n"
        "    WHERE x.id = (j->'train'->>'id')::bigint\n"
        "      AND to_jsonb(x) = j->'train')) AS trains_identical,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'events') e)\n"
        "    AS events_expected,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'events') e\n"
        "    WHERE EXISTS (SELECT 1 FROM train_movement_events x\n"
        "      WHERE x.id = (e->>'id')::bigint\n"
        "        AND to_jsonb(x) = e)) AS events_identical,\n"
        "  count(*) FILTER (WHERE jsonb_typeof(j->'state') = 'object')\n"
        "    AS state_expected,\n"
        "  count(*) FILTER (WHERE jsonb_typeof(j->'state') = 'object'\n"
        "    AND EXISTS (SELECT 1 FROM train_current_state x\n"
        "      WHERE x.id = (j->'state'->>'id')::bigint\n"
        "        AND to_jsonb(x) = j->'state')) AS state_identical,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'reasons') e)\n"
        "    AS reasons_expected,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'reasons') e\n"
        "    WHERE EXISTS (SELECT 1 FROM train_reasons x\n"
        "      WHERE x.trains_id = (e->>'trains_id')::bigint\n"
        "        AND x.msg_type = e->>'msg_type'\n"
        "        AND to_jsonb(x) = e)) AS reasons_identical,\n"
        "  count(*) FILTER (WHERE jsonb_typeof(j->'label') = 'object')\n"
        "    AS labels_expected,\n"
        "  count(*) FILTER (WHERE jsonb_typeof(j->'label') = 'object'\n"
        "    AND EXISTS (SELECT 1 FROM train_service_labels x\n"
        "      WHERE x.trains_id = (j->'label'->>'trains_id')::bigint\n"
        "        AND to_jsonb(x) = j->'label')) AS labels_identical,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'subs') s)\n"
        "    AS subscriptions_expected,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'subs') s\n"
        "    WHERE EXISTS (SELECT 1 FROM train_subscriptions x\n"
        "      WHERE x.id = (s #>> '{}')::bigint\n"
        "        AND x.trains_id = (d2.j->'train'->>'id')::bigint))\n"
        "    AS subscriptions_linked\n"
        "FROM d;\n"
    )


def rollback_sql(bundles: Sequence[Bundle], run_at: dt.datetime) -> str:
    """Undo: unlink the subscriptions, delete the restored trains.

    Children go with the train (their FKs cascade). A train is deleted only
    if no other subscription points at it. It matches on id, uid and date, so
    it also removes whatever has been written under those since (the
    aggregator updating the row, new events).
    """
    lines = [
        (
            f"-- Undo run {rfc3339(run_at)}: removes the restored trains and "
            "their\n-- children (cascade), and unlinks the subscriptions this "
            "run linked.\nBEGIN;"
        ),
        "SET LOCAL lock_timeout = '5s';",
    ]
    for bundle in bundles:
        subs = int_array(bundle.subs)
        lines.append(
            "UPDATE train_subscriptions SET trains_id = NULL "
            f"WHERE id = ANY({subs}) AND trains_id = {bundle.train_id};"
        )
        lines.append(
            "DELETE FROM trains t "
            f"WHERE t.id = {bundle.train_id} AND t.train_uid = "
            f"{dollar_quote(bundle.uid)} AND t.service_date = "
            f"'{bundle.date}'::date AND NOT EXISTS "
            "(SELECT 1 FROM train_subscriptions s WHERE s.trains_id = t.id);"
        )
    lines.append("COMMIT;")
    return "\n".join(lines) + "\n"


def preflight_sql(bundles: Sequence[Bundle]) -> str:
    """Read-only SQL for the LIVE database: counts only, no ids or uids out."""
    docs = [
        {
            "id": b.train_id,
            "uid": b.uid,
            "date": b.date,
            "trainId": b.train["train_id"],
            "subs": b.subs,
            "events": [e["id"] for e in b.events],
            "state": b.state["id"] if b.state is not None else None,
        }
        for b in bundles
    ]
    lit = f"{dollar_quote(dumps(docs))}::jsonb"
    top = max_ids(bundles)
    live = "x.id = (j->>'id')::bigint"
    same = "x.train_uid = j->>'uid' AND x.service_date = (j->>'date')::date"
    return (
        "-- Read-only, counts only: run on the LIVE database before the batch.\n"
        "-- A train is inserted only if it is in none of id_collision,\n"
        "-- live_row_exists and child_id_taken (they can overlap) and is not\n"
        "-- already_restored (a no-op, the relink still applies).\n"
        "SET TimeZone = 'UTC';\n"
        f"WITH d AS (SELECT j FROM jsonb_array_elements({lit}) AS j)\n"
        "SELECT count(*) AS trains,\n"
        f"  count(*) FILTER (WHERE EXISTS (SELECT 1 FROM trains x WHERE {live}\n"
        f"    AND {same})) AS already_restored,\n"
        f"  count(*) FILTER (WHERE EXISTS (SELECT 1 FROM trains x WHERE {live}\n"
        f"    AND NOT ({same}))) AS id_collision,\n"
        "  count(*) FILTER (WHERE EXISTS (SELECT 1 FROM trains x\n"
        f"    WHERE x.id <> (j->>'id')::bigint AND ({same}\n"
        "      OR (j->>'trainId' IS NOT NULL AND x.train_id = j->>'trainId'\n"
        "        AND x.service_date = (j->>'date')::date)))) AS live_row_exists,\n"
        "  count(*) FILTER (WHERE NOT EXISTS (SELECT 1 FROM trains x WHERE\n"
        f"    {live}) AND (EXISTS (SELECT 1 FROM train_movement_events e\n"
        "      WHERE e.id IN (SELECT (v #>> '{}')::bigint\n"
        "        FROM jsonb_array_elements(j->'events') v))\n"
        "    OR EXISTS (SELECT 1 FROM train_current_state s\n"
        "      WHERE s.id = (j->>'state')::bigint))) AS child_id_taken,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'subs') s)\n"
        "    AS subscriptions,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'subs') s\n"
        "    JOIN train_subscriptions ts ON ts.id = (s #>> '{}')::bigint\n"
        "    WHERE ts.trains_id IS NULL\n"
        "      AND ts.service_date = (d2.j->>'date')::date)\n"
        "    AS subscriptions_relinkable,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'subs') s\n"
        "    JOIN train_subscriptions ts ON ts.id = (s #>> '{}')::bigint\n"
        "    WHERE ts.trains_id IS NOT NULL) AS subscriptions_already_linked,\n"
        "  (SELECT count(*) FROM d d2, jsonb_array_elements(d2.j->'subs') s\n"
        "    WHERE NOT EXISTS (SELECT 1 FROM train_subscriptions ts\n"
        "      WHERE ts.id = (s #>> '{}')::bigint)) AS subscriptions_missing\n"
        "FROM d;\n"
        "\n"
        "-- Headroom: every *_ok must be true (no setval is needed).\n"
        "SELECT\n"
        "  COALESCE((SELECT max(id) FROM trains), 0)\n"
        f"    > {top[0]} AS trains_ids_ok,\n"
        "  COALESCE((SELECT max(id) FROM train_movement_events), 0)\n"
        f"    > {top[1]}\n"
        "    AS events_ids_ok,\n"
        "  COALESCE((SELECT max(id) FROM train_current_state), 0)\n"
        f"    > {top[2]}\n"
        "    AS state_ids_ok,\n"
        + ",\n".join(
            "  (SELECT string_agg(column_name::text, ',' ORDER BY column_name) "
            "FROM information_schema.columns WHERE table_schema = 'public' "
            f"AND table_name = '{table}') = '{','.join(sorted(cols))}' "
            f"AS {table}_columns_ok"
            for table, cols in TABLES.items()
        )
        + ";\n"
    )


# --- emit ------------------------------------------------------------------


def total_counts(bundles: Sequence[Bundle]) -> Counter[str]:
    """Row counts of what would be inserted."""
    counts: Counter[str] = Counter()
    for b in bundles:
        counts["trains"] += 1
        counts["train_movement_events"] += len(b.events)
        counts["train_current_state"] += b.state is not None
        counts["train_reasons"] += len(b.reasons)
        counts["train_service_labels"] += b.label is not None
        counts["subscriptions"] += len(b.subs)
    return counts


def emit(
    out_dir: Path,
    bundles: Sequence[Bundle],
    header: Row,
    run_at: dt.datetime,
    size: int,
) -> tuple[int, int]:
    """Write batches, verify, rollback, preflight and the manifest.

    The manifest holds counts and checksums only (no ids).

    Returns the number of files written and their total size in bytes.
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    texts: dict[str, str] = {}
    for number, text in enumerate(batches(bundles, size), start=1):
        texts[f"batch-{number:04d}.sql"] = text
    texts["preflight.sql"] = preflight_sql(bundles)
    texts["verify.sql"] = verify_sql(bundles)
    texts["rollback.sql"] = rollback_sql(bundles, run_at)
    files = []
    for name, text in texts.items():
        data = text.encode()
        write_private(out_dir / name, data)
        files.append(
            {
                "name": name,
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }
        )
    manifest: dict[str, object] = {
        "script": "extract-pitr-trains.py",
        "scriptVersion": SCRIPT_VERSION,
        "target": header.get("target"),
        "generatedAt": rfc3339(run_at),
        "batchSize": size,
        "extractCounts": header.get("counts"),
        "toInsert": dict(total_counts(bundles)),
        "totalBytes": sum(int(str(f["bytes"])) for f in files),
        "files": files,
    }
    write_private(
        out_dir / "manifest.json", (dumps(manifest, indent=2) + "\n").encode()
    )
    return len(files) + 1, int(str(manifest["totalBytes"]))


# --- commands --------------------------------------------------------------


def n(value: int) -> str:
    """Return a count with thousands separators."""
    return f"{value:,}"


def report_counts(header: Row, bundles: Sequence[Bundle]) -> None:
    """Print counts only."""
    raw = header.get("counts")
    counts = raw if isinstance(raw, dict) else {}
    print(f"target {header.get('target')}")
    for key in (
        "targets",
        "distinct_trains",
        "extracted",
        "no_match",
        "ambiguous",
        "rejected",
    ):
        print(f"  {key:<24}{n(int(counts.get(key, 0)))}")
    inserts = total_counts(bundles)
    for key, value in inserts.items():
        print(f"  would insert {key:<18}{n(value)}")


def run_extract(args: argparse.Namespace) -> int:
    """Read the scratch; write the extract file; optionally the SQL."""
    targets, other = read_targets(Path(args.targets), args.target)
    if not targets:
        message = f"no {args.target} lines in {args.targets}"
        raise SystemExit(message)
    run_at = dt.datetime.now(dt.UTC)
    ex = extract_target(targets, args.psql, verbose=args.verbose, target=args.target)
    write_extract(Path(args.out), ex, run_at)
    report_counts(
        {"target": args.target, "counts": dict(ex.counts)},
        ex.bundles,
    )
    print(f"  other-target lines ignored  {n(other)}")
    if args.verbose:
        for outcome, subs in sorted(ex.outcome_subs.items()):
            print(f"  {outcome}: subscriptions {sorted(subs)}")
    if args.emit_sql:
        files, size = emit(
            Path(args.emit_sql),
            ex.bundles,
            {"target": args.target, "counts": dict(ex.counts)},
            run_at,
            args.batch_size,
        )
        print(f"wrote {n(files)} files, {n(size)} bytes, to {args.emit_sql}")
    else:
        print("nothing emitted (add --emit-sql DIR, or run `emit`)")
    return 0


def run_plan(args: argparse.Namespace) -> int:
    """Print counts only, from an extract file."""
    header, bundles = read_extract(Path(args.extract))
    report_counts(header, bundles)
    return 0


def run_preflight(args: argparse.Namespace) -> int:
    """Write the live-database preflight SQL (it names trains: mode 0600)."""
    _, bundles = read_extract(Path(args.extract))
    write_private(Path(args.out), preflight_sql(bundles).encode())
    print(f"wrote {args.out} ({n(len(bundles))} trains): run it read-only on live")
    return 0


def run_emit(args: argparse.Namespace) -> int:
    """Write the SQL directory from an extract file."""
    header, bundles = read_extract(Path(args.extract))
    files, size = emit(
        Path(args.emit_sql), bundles, header, dt.datetime.now(dt.UTC), args.batch_size
    )
    print(f"wrote {n(files)} files, {n(size)} bytes, to {args.emit_sql}")
    return 0


def parser() -> argparse.ArgumentParser:
    """Build the command line."""
    top = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = top.add_subparsers(dest="command", required=True)

    ex = sub.add_parser("extract", help="read the scratch Postgres (read-only)")
    ex.add_argument("--targets", required=True, help="targets.jsonl")
    ex.add_argument("--target", required=True, choices=TARGETS)
    ex.add_argument(
        "--psql",
        required=True,
        help="psql command prefix reaching the scratch, e.g. "
        "'kubectl exec -i POD -- psql -X -At -U postgres -d distant_signal'",
    )
    ex.add_argument("--out", required=True, help="extract file to write (mode 0600)")
    ex.add_argument("--emit-sql", metavar="DIR", help="also write the SQL here")
    ex.add_argument("--batch-size", type=int, default=DEFAULT_BATCH)
    ex.add_argument("--verbose", action="store_true", help="print opaque ids")
    ex.set_defaults(func=run_extract)

    plan = sub.add_parser("plan", help="counts only, from an extract file")
    plan.add_argument("--extract", required=True)
    plan.set_defaults(func=run_plan)

    pre = sub.add_parser("preflight", help="write the live read-only SQL")
    pre.add_argument("--extract", required=True)
    pre.add_argument("--out", required=True)
    pre.set_defaults(func=run_preflight)

    emit_cmd = sub.add_parser("emit", help="write the SQL directory")
    emit_cmd.add_argument("--extract", required=True)
    emit_cmd.add_argument("--emit-sql", metavar="DIR", required=True)
    emit_cmd.add_argument("--batch-size", type=int, default=DEFAULT_BATCH)
    emit_cmd.set_defaults(func=run_emit)
    return top


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point."""
    args = parser().parse_args(argv)
    if getattr(args, "batch_size", 1) < 1:
        message = "--batch-size must be at least 1"
        raise SystemExit(message)
    return int(args.func(args))


if __name__ == "__main__":
    with contextlib.suppress(BrokenPipeError):
        try:
            sys.exit(main())
        except SystemExit as exc:
            if isinstance(exc.code, str):
                print(exc.code, file=sys.stderr)
                sys.exit(1)
            raise
