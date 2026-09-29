"""`susq` command-line entry point (research + ops helpers)."""

import typer

from susq.scheduler import cli_app as research_app

app = typer.Typer(no_args_is_help=True, help="Susquatch research and ops CLI.")
app.add_typer(research_app, name="research")


@app.callback()
def _root() -> None:
    """Group callback so sub-commands stay sub-commands."""


@app.command()
def version() -> None:
    """Print the package version."""
    from susq import __version__

    typer.echo(__version__)
