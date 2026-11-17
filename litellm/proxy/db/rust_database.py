"""The worker's Rust database handle and the auth stores built over it.

``Database.open`` installs the ``NativeDatabase`` it opened and ``Database.aclose`` clears it. Auth lookups ask
``store`` for their Rust store on a cache miss; each store is built once per installed database, so a store's
own memory tier lives as long as the pool does.
"""

from collections.abc import Callable
from typing import Final, TypeVar

from litellm.rust_bridge._native import NativeDatabase

StoreT = TypeVar("StoreT")


class _Installed:
    __slots__ = ("database", "stores")

    def __init__(self) -> None:
        self.database: NativeDatabase | None = None
        self.stores: Final[dict[Callable[[NativeDatabase], object], object]] = {}  # mutable-ok: one store per kind


_installed: Final = _Installed()


def install(database: NativeDatabase) -> None:
    _installed.stores.clear()
    _installed.database = database


def clear(database: NativeDatabase) -> None:
    """Forgets ``database`` and its stores, unless another database was installed since."""
    if _installed.database is database:
        _installed.stores.clear()
        _installed.database = None


def current() -> NativeDatabase | None:
    return _installed.database


def store(kind: Callable[[NativeDatabase], StoreT]) -> StoreT | None:
    """The ``kind`` store over the installed database, or None when no Rust database is open."""
    database: Final = _installed.database
    if database is None:
        return None
    existing: Final = _installed.stores.get(kind)
    if existing is not None:
        return existing  # pyright: ignore[reportReturnType]  # stored under its own kind
    built: Final = kind(database)
    _installed.stores[kind] = built
    return built
