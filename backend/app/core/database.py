from sqlalchemy import create_engine, event
from sqlalchemy.orm import declarative_base, sessionmaker
from sqlalchemy.pool import NullPool, StaticPool

from app.core.config import settings

# One codebase, two databases. The hosted deployment runs Postgres; a
# self-hosted install runs SQLite (the `DATABASE_URL` default in
# config.py). Everything below branches on which one we got, because
# almost none of the SQLite tuning is valid on Postgres — `PRAGMA` is a
# syntax error there, and `check_same_thread` / `timeout` are pysqlite
# connect kwargs that psycopg rejects outright.
_is_sqlite = settings.DATABASE_URL.startswith("sqlite")

# In-memory SQLite needs StaticPool (one shared connection) or every
# operation sees a different empty database. Test-suite only.
_is_memory_db = _is_sqlite and ":memory:" in settings.DATABASE_URL

if _is_sqlite:
    # NullPool: each request gets a fresh connection and releases it
    # immediately — the recommended approach for SQLite under the bursty
    # concurrency of HLS segment uploads (15+ at once), where a real pool
    # just queues on the single writer anyway.
    engine = create_engine(
        settings.DATABASE_URL,
        connect_args={
            "check_same_thread": False,
            **({} if _is_memory_db else {"timeout": 30}),
        },
        poolclass=StaticPool if _is_memory_db else NullPool,
    )
else:
    # Postgres keeps SQLAlchemy's default QueuePool. NullPool would be
    # actively harmful here: every request would pay a fresh TCP + TLS +
    # auth round trip to a database that is now over the network rather
    # than on local disk.
    #
    # pool_pre_ping issues a cheap liveness check before handing out a
    # pooled connection, so a managed provider recycling idle connections
    # surfaces as a transparent reconnect instead of a request failing on
    # a dead socket.
    engine = create_engine(settings.DATABASE_URL, pool_pre_ping=True)


# Registered conditionally rather than early-returning inside the
# handler: on Postgres this must never fire at all, since `PRAGMA` is a
# syntax error and would break every single connect.
if _is_sqlite:

    @event.listens_for(engine, "connect")
    def set_sqlite_pragma(dbapi_conn, connection_record):
        """WAL + foreign keys for SQLite.

        WAL allows concurrent readers while writing, preventing lock
        contention from the heavy HLS segment upload traffic.
        """
        cursor = dbapi_conn.cursor()
        cursor.execute("PRAGMA journal_mode=WAL")
        cursor.execute("PRAGMA foreign_keys=ON")
        # 30_000 ms, matching connect_args' timeout=30 above. These two
        # MUST agree: pysqlite sets its busy timeout from the connect
        # arg, then this PRAGMA runs immediately after and overrides it,
        # so the PRAGMA is what actually takes effect. They disagreed
        # until 2026-09-07 (connect said 30s, PRAGMA said 5s, 5s won).
        #
        # 30s rather than fail-fast because of who actually contends.
        # The hot path — push_segment at 1200/min — performs NO writes;
        # it does auth reads and puts segments in the RAM cache, and WAL
        # never blocks readers behind the writer. The writers are the
        # background loops (log cleanup's bulk deletes, offline sweep,
        # viewer-usage flush). Writer-vs-writer there is exactly the case
        # where waiting beats erroring: a short timeout turns a heartbeat
        # or motion-event insert into a "database is locked" failure and
        # a permanently lost row, while cleanup holds the write lock.
        #
        # Self-hosted only since the hosted deployment moved to Postgres.
        cursor.execute("PRAGMA busy_timeout=30000")
        # NORMAL is the standard WAL pairing: an fsync per checkpoint
        # instead of per COMMIT.  Default FULL was fsyncing every commit —
        # heartbeats (every 30s x every node), motion events, access logs —
        # each holding the event loop 1-10ms on the Fly volume.  Worst case
        # on power loss is the last few commits; WAL guarantees no
        # corruption either way.
        #
        # Postgres needs no equivalent: foreign keys are always enforced,
        # and its WAL is not optional.
        cursor.execute("PRAGMA synchronous=NORMAL")
        cursor.close()


SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=engine)

Base = declarative_base()


def get_db():
    db = SessionLocal()
    try:
        yield db
    finally:
        db.close()
