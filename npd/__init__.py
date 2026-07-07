"""npd — a persistent fact store for iterating on nixpkgs changes.

See DESIGN.md for the architecture. The pure data model lives in `npd.model`;
orchestration (eval / diff / build / hydra / report) is being built spine-first.
"""

__version__ = "0.0.0"
