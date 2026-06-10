from sqlalchemy import create_engine, event
from sqlalchemy.orm import declarative_base, sessionmaker
from sqlalchemy.pool import NullPool, StaticPool

from app.core.config import settings

# NullPool: each request gets a fresh connection and releases it immediately.
# This is the recommended approach for SQLite in async/high-concurrency apps —
# avoids pool exhaustion when HLS segment uploads create bursts of 15+ concurrent requests.
#
# Exception: in-memory SQLite requires StaticPool (shared single connection)
# so that all operations see the same database.
_is_memory_db = settings.DATABASE_URL == "sqlite:///:memory:" or ":memory:" in settings.DATABASE_URL

engine = create_engine(
    settings.DATABASE_URL,
    connect_args={"check_same_thread": False, **({} if _is_memory_db else {"timeout": 30})},
    poolclass=StaticPool if _is_memory_db else NullPool,
)


# Enable WAL mode + foreign keys for SQLite.
# WAL allows concurrent readers while writing, preventing lock contention
# from the heavy HLS segment upload traffic.
@event.listens_for(engine, "connect")
def set_sqlite_pragma(dbapi_conn, connection_record):
    cursor = dbapi_conn.cursor()
    cursor.execute("PRAGMA journal_mode=WAL")
    cursor.execute("PRAGMA foreign_keys=ON")
    cursor.execute("PRAGMA busy_timeout=5000")
    # NORMAL is the standard WAL pairing: an fsync per checkpoint
    # instead of per COMMIT.  Default FULL was fsyncing every commit —
    # heartbeats (every 30s x every node), motion events, access logs —
    # each holding the event loop 1-10ms on the Fly volume.  Worst case
    # on power loss is the last few commits; WAL guarantees no
    # corruption either way.
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
