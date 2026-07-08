from typing import TYPE_CHECKING, Any, List, Optional, Tuple, Union
from uuid import UUID

if TYPE_CHECKING:
    from sqlfluff.core.config import FluffConfig
    from sqlfluff.core.parser.lexer import StringLexer
    from sqlfluff.core.parser.segments import SourceFix
    from sqlfluff.core.templaters import TemplatedFile

SerializedObject = dict[str, Union[str, int, bool, list["SerializedObject"]]]
TupleSerialisedSegment = tuple[str, Union[str, tuple["TupleSerialisedSegment", ...]]]

class Slice: ...

class RsRawFileSlice:
    raw: str
    slice_type: str
    source_idx: int
    block_idx: int
    tag: Optional[str]

class RsTemplatedFileSlice:
    slice_type: str
    source_slice: Slice
    templated_slice: Slice

class RsTemplatedFile:
    source_str: str
    fname: str
    templated_str: str
    sliced_file: List[RsTemplatedFileSlice]
    raw_sliced: List[RsRawFileSlice]

class RsPositionMarker:
    source_slice: slice
    templated_slice: slice
    templated_file: RsTemplatedFile
    working_line_no: int
    working_line_pos: int

class RsToken:
    raw: str
    pos_marker: RsPositionMarker
    type: str
    uuid: Optional[int]
    source_fixes: Optional[list["SourceFix"]]

    def raw_trimmed(self) -> str: ...
    @property
    def is_templated(self) -> bool: ...
    @property
    def is_code(self) -> bool: ...
    @property
    def is_meta(self) -> bool: ...
    @property
    def source_str(self) -> str: ...
    @property
    def block_type(self) -> str: ...
    @property
    def block_uuid(self) -> Optional[UUID]: ...
    @property
    def cache_key(self) -> str: ...
    @property
    def trim_start(self) -> Optional[tuple[str]]: ...
    @property
    def trim_chars(self) -> Optional[tuple[str]]: ...
    @property
    def quoted_value(self) -> Optional[tuple[str, int | str]]: ...
    @property
    def escape_replacements(self) -> Optional[list[tuple[str, str]]]: ...
    def count_segments(self, raw_only: bool = False) -> int: ...
    def get_type(self) -> str: ...
    def recursive_crawl(
        self,
        seg_type: Tuple[str, ...],
        recurse_into: bool,
        no_recursive_seg_type: Optional[Union[str, List[str]]] = None,
        allow_self: bool = True,
    ) -> List["RsToken"]: ...
    def recursive_crawl_all(self, reverse: bool) -> List["RsToken"]: ...
    @property
    def segments(self) -> List["RsToken"]: ...
    def path_to(self, other: "RsToken") -> List[Any]: ...
    def get_start_loc(self) -> Tuple[int, int]: ...
    def get_end_loc(self) -> Tuple[int, int]: ...
    @property
    def raw_segments(self) -> List["RsToken"]: ...
    def copy(
        self,
        segments: Optional[List["RsToken"]] = None,
        parent: Optional[Any] = None,
        parent_idx: Optional[int] = None,
    ) -> "RsToken": ...
    def edit(
        self,
        raw: Optional[str] = None,
        source_fixes: Optional[List[Any]] = None,
    ) -> "RsToken": ...
    def to_tuple(
        self,
        code_only: Optional[bool] = None,
        show_raw: Optional[bool] = None,
        include_meta: Optional[bool] = None,
    ) -> TupleSerialisedSegment: ...
    def __repr__(self) -> str: ...
    @property
    def instance_types(self) -> List[str]: ...
    @staticmethod
    def template_placeholder_from_slice(
        source_slice: tuple[int, int],
        templated_slice: tuple[int, int],
        block_type: str,
        _source_str: str,
        block_uuid: Optional[str],
        templated_file: "TemplatedFile",
    ) -> "RsToken": ...

class RsNode: ...

class RsSQLLexerError:
    desc: str
    line_no: int
    line_pos: int
    ignore: bool
    warning: bool
    fatal: bool

    def __init__(
        self,
        msg: Optional[str] = None,
        pos: Optional[RsPositionMarker] = None,
        line_no: int = 0,
        line_pos: int = 0,
        ignore: bool = False,
        warning: bool = False,
        fatal: bool = False,
    ) -> None: ...
    def rule_code(self) -> str: ...
    def rule_name(self) -> str: ...
    def source_signature(self) -> Tuple[Tuple[str, int, int], str]: ...
    def to_dict(self) -> SerializedObject: ...
    def ignore_if_in(self, ignore_iterable: list[str]) -> None: ...
    def warning_if_in(self, ignore_iterable: list[str]) -> None: ...

class RsLexer:
    def __init__(
        self,
        config: Optional["FluffConfig"] = None,
        last_resort_lexer: Optional["StringLexer"] = None,
        dialect: Optional[str] = None,
    ): ...
    def _lex(
        self, lex_input: Union[str, "TemplatedFile"]
    ) -> Tuple[List[RsToken], List[Any]]: ...

class RsMatchResult:
    """Result of a Rust parser match operation."""

    matched_slice: tuple[int, int]
    matched_class: Optional[str]
    child_matches: List["RsMatchResult"]
    parse_error: Optional[tuple[str, int]]
    instance_types: Optional[List[str]]
    segment_kwargs: Optional[dict[str, Any]]
    trim_chars: Optional[List[str]]
    casefold: Optional[str]
    quoted_value: Optional[str]
    escape_replacement: Optional[tuple[str, str]]
    insert_segments: Optional[List[tuple[int, str, bool]]]

    def apply_as_tree(
        self,
        tokens: List[RsToken],
        leading: List[RsToken],
        trailing: List[RsToken],
    ) -> "RsTree": ...

class RsHandle:
    """A lightweight cursor into an :class:`RsTree` arena node.

    Every accessor runs Rust-side; only thin handles and scalars cross FFI.
    Wrapped by the Python ``RsSegment`` facade.
    """

    uuid: int
    raw: str
    raw_upper: str
    type: str
    segment_class: Optional[str]
    is_code: bool
    is_whitespace: bool
    is_comment: bool
    is_meta: bool
    is_templated: bool
    pos_marker: Optional[RsPositionMarker]
    children: List["RsHandle"]
    parent: Optional["RsHandle"]

    def is_type(self, seg_type: List[str]) -> bool: ...
    def is_raw(self) -> bool: ...
    def class_types(self) -> List[str]: ...
    def instance_types(self) -> List[str]: ...
    def is_implicit(self) -> Optional[bool]: ...
    def trim_chars(self) -> Optional[List[str]]: ...
    def quoted_value(self) -> Optional[tuple[str, str]]: ...
    def escape_replacements(self) -> Optional[List[tuple[str, str]]]: ...
    def descendant_type_set(self) -> List[str]: ...
    def get_parent(self) -> Optional[tuple["RsHandle", int]]: ...
    def get_child(self, seg_type: List[str]) -> Optional["RsHandle"]: ...
    def get_children(self, seg_type: List[str]) -> List["RsHandle"]: ...
    def raw_segments(self) -> List["RsHandle"]: ...
    def recursive_crawl(
        self,
        seg_type: List[str],
        recurse_into: bool = True,
        no_recursive_seg_type: List[str] = ...,
        allow_self: bool = True,
    ) -> List["RsHandle"]: ...
    def recursive_crawl_all(self) -> List["RsHandle"]: ...
    def path_to(
        self, other: "RsHandle"
    ) -> List[tuple["RsHandle", int, int, List[int]]]: ...
    def source_fixes(
        self,
    ) -> List[tuple[str, tuple[int, int], tuple[int, int]]]:
        """Subtree source fixes in document order.

        ``(edit, (source_start, source_stop), (templated_start,
        templated_stop))`` tuples — mirrors ``BaseSegment.source_fixes``.
        """
        ...

    def source_str(self) -> Optional[str]:
        """Stored ``source_str`` of a Template placeholder meta; else None."""
        ...

    def is_detached(self) -> bool:
        """True once tombstoned (unlinked) by an edit batch."""
        ...

class RsTree:
    """Owner of a mutable arena parse tree, used by the ``RsSegment`` facade."""

    root: RsHandle
    # The Python ``TemplatedFile`` the engine rendered before parsing (``None``
    # if the tree was not built via an engine render). Passed as
    # ``context.templated_file`` by the facade linting path.
    templated_file: Optional[Any]
    # Templater violations from the engine render (``None`` when not rendered;
    # an empty list for a clean render). Native lint reports these, so a
    # violation-bearing render must route to native.
    templater_violations: Optional[list[Any]]
    # Variants the render produced. Native lints EVERY variant and merges.
    num_variants: int
    # Arena trees for the render's ALTERNATE variants (root variant is this
    # tree), each carrying its own ``templated_file``. Variants that failed
    # to parse are omitted (native's ``not alternate_variant.tree`` skip).
    alternate_trees: list["RsTree"]
    # Mutation epoch — bumped once per committed edit batch. Python wrapper
    # caches key their validity off this.
    epoch: int

    def __len__(self) -> int: ...
    def node_by_uuid(self, uuid: int) -> Optional[RsHandle]: ...
    def stage_edit_batch(
        self, ops: List[tuple], fix_even_unparsable: bool
    ) -> tuple[
        str,
        List[tuple[str, tuple[int, int], tuple[int, int]]],
        int,
        List[int],
        int,
        bool,
    ]:
        """Plan an edit batch WITHOUT mutating.

        ``ops`` is ``[(anchor_uuid, kind, edits), …]`` (see arena_py.rs).
        Returns ``(staged_raw, staged_source_fixes, applied,
        unapplied_anchors, reverted_containers, changed)`` so the fix loop can
        run native's loop-detection gates on the predicted state.
        """
        ...

    def commit_staged(self) -> int:
        """Install the staged plan; returns the new epoch. Errors if none."""
        ...

    def discard_staged(self) -> None:
        """Drop the staged plan without mutating."""
        ...

    def has_staged(self) -> bool: ...
    def validate_staged(
        self, dialect: str, max_parse_depth: int, max_parse_nodes: int
    ) -> bool:
        """Whether the staged plan re-matches its own grammar.

        Rust analogue of native ``validate_segment_with_reparse``; returns
        ``True`` when nothing is staged. ``max_parse_depth`` / ``max_parse_nodes``
        are the file's configured parse ceilings (``0`` disables a limit).
        """
        ...

class RsParseError(Exception):
    """Exception raised by Rust parser when parsing fails.

    Attributes:
        pos: Position index in the segments array where the error occurred
    """

    pos: int

class RsParser:
    """Rust-based SQL parser."""

    def __init__(
        self,
        dialect: str,
        indent_config: Optional[dict[str, bool]] = None,
        max_parser_iterations: Optional[int] = None,
        parser_warn_threshold: Optional[int] = None,
        max_parse_depth: int = 0,
        max_parse_nodes: int = 0,
    ): ...
    def parse_match_result_from_tokens(
        self, tokens: List[RsToken]
    ) -> RsMatchResult: ...

def engine_parse_paths(
    paths: list[str],
    config: "FluffConfig",
    formatter: Any = None,
    *,
    stdin_content: Optional[str] = None,
    stdin_filename: Optional[str] = None,
    code_only: bool = False,
    include_meta: bool = False,
    parse_statistics: bool = False,
) -> list[dict[str, Any]]:
    """Rust-driven discover->render->lex->parse for the `parse` command.

    Returns one dict per file: `{fname, segments, templater_violations,
    lex_errors, parse_errors}`.
    """
    ...

def engine_render_string(
    raw_sql: str,
    fname: str,
    config: "FluffConfig",
    formatter: Any = None,
) -> dict[str, Any]:
    """Rust-driven render for the `render` command.

    Returns `{templated_variants, templater_violations}`.
    """
    ...

def engine_parse_to_tree(
    raw_sql: str,
    fname: str,
    config: "FluffConfig",
    formatter: Any = None,
    direct_config: bool = False,
) -> Optional["RsTree"]:
    """Rust-driven parse of one file to a crawlable arena tree (`RsTree`).

    Built from the native `Node` via `Arena::from_node`, for linting the Python
    rules over an `RsSegment` façade. `None` if render/parse produced no tree.
    """
    ...

def cp01_violations(
    tree: RsTree,
    policy: str,
    ignore_words: List[str] = ...,
    ignore_templated: bool = ...,
) -> List[Tuple[int, str]]:
    """Detect CP01 (keyword capitalisation) violations natively over the arena.

    Returns ``(leaf_index, fixed_raw)`` pairs; ``leaf_index`` is 1:1 with the
    parse tree's ``raw_segments`` order.
    """
    ...
