"""Path checks shared by the on-demand historical importers."""

from pathlib import Path


def _is_checkout(path):
    marker = path / ".git"
    return marker.is_file() or (marker / "HEAD").is_file()


def validate_history_root(data_root):
    root = Path(data_root)
    if not root.is_absolute():
        raise ValueError("historical data root must be an explicit absolute path")
    root = root.resolve()
    checkout = Path(__file__).resolve().parents[2]
    if root.is_relative_to(checkout) or any(_is_checkout(parent) for parent in (root, *root.parents)):
        raise ValueError("historical data root must be outside a Git working tree")
    return root


def history_path(root, *components):
    """Join checked path components below a validated root without following symlinks."""
    path = root
    for component in components:
        if not component or component in {".", ".."} or Path(component).name != component:
            raise ValueError("invalid historical path component")
        path = path / component
        if path.is_symlink() or _is_checkout(path):
            raise ValueError("historical paths must stay inside the configured data root")
    return path
