"""`susq-bridge` command-line entry point."""

import typer

app = typer.Typer(no_args_is_help=True, help="Susquatch Oct-1 Python bridge.")


@app.callback()
def _root() -> None:
    """Group callback so sub-commands stay sub-commands."""


@app.command()
def version() -> None:
    """Print the bridge version."""
    from susq_bridge import __version__

    typer.echo(__version__)
