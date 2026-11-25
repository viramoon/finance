"""Finance batch jobs and Python research tools."""

__all__ = ["run_backtest"]


def __getattr__(name):
    if name == "run_backtest":
        from .research_backtest import run_backtest

        return run_backtest
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
