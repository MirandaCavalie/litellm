from collections.abc import Iterator
from typing import cast

import pytest

from litellm.proxy.db import rust_database
from litellm.rust_bridge._native import NativeDatabase


class _Store:
    def __init__(self, database: NativeDatabase) -> None:
        self.database = database


def _database() -> NativeDatabase:
    return cast(NativeDatabase, object())


@pytest.fixture(autouse=True)
def _reset() -> Iterator[None]:
    yield
    database = rust_database.current()
    if database is not None:
        rust_database.clear(database)


def test_store_is_none_without_a_database() -> None:
    assert rust_database.current() is None
    assert rust_database.store(_Store) is None


def test_store_is_built_once_per_installed_database() -> None:
    database = _database()
    rust_database.install(database)

    first = rust_database.store(_Store)

    assert rust_database.current() is database
    assert first is not None and first.database is database
    assert rust_database.store(_Store) is first


def test_install_drops_stores_of_the_previous_database() -> None:
    old = _database()
    rust_database.install(old)
    old_store = rust_database.store(_Store)
    new = _database()

    rust_database.install(new)
    new_store = rust_database.store(_Store)

    assert new_store is not old_store
    assert new_store is not None and new_store.database is new


def test_clear_forgets_the_database_and_its_stores() -> None:
    database = _database()
    rust_database.install(database)
    rust_database.store(_Store)

    rust_database.clear(database)

    assert rust_database.current() is None
    assert rust_database.store(_Store) is None


def test_clear_keeps_a_newer_database() -> None:
    old = _database()
    new = _database()
    rust_database.install(old)
    rust_database.install(new)
    kept = rust_database.store(_Store)

    rust_database.clear(old)

    assert rust_database.current() is new
    assert rust_database.store(_Store) is kept
