import tempfile
from pathlib import Path

from validation.interop.host_contract import (
    ROOT,
    build_host_library,
    environment,
    host_c_debug_target,
    host_contract_main,
    package_host_native,
    run_command,
    swift_host_target,
)


SUCCESS = "HOST_SWIFT_CONTRACT_SMOKE_OK"
XCODE_DEVELOPER = Path("/Applications/Xcode.app/Contents/Developer")


def swift_environment(values: dict[str, object]) -> dict[str, str]:
    """Prefer full Xcode when present so `import Testing` resolves.

    Command Line Tools alone ship a Swift driver without the Testing module,
    which makes this smoke fail even though the host libraries built correctly.
    """
    configured = environment(values)
    if "DEVELOPER_DIR" not in configured and XCODE_DEVELOPER.is_dir():
        configured["DEVELOPER_DIR"] = str(XCODE_DEVELOPER)
    return configured


def run() -> None:
    build_host_library()
    target = swift_host_target()
    with tempfile.TemporaryDirectory(prefix="prns-host-swift-") as temporary:
        scratch = Path(temporary)
        native = scratch / "native"
        package_host_native(
            output=native,
            rust_target=target.rust_target,
            dynamic_library=host_c_debug_target() / target.dynamic_library_name,
        )
        run_command(
            (
                "swift",
                "test",
                "--package-path",
                ROOT / "prns-host/bindings/swift",
                "--scratch-path",
                scratch / "build",
            ),
            "Swift host contract smoke failed",
            command_environment=swift_environment(
                {
                    "PKG_CONFIG_PATH": native / "lib/pkgconfig",
                    "LD_LIBRARY_PATH": native / "lib",
                    "DYLD_LIBRARY_PATH": native / "lib",
                    "CLANG_MODULE_CACHE_PATH": scratch / "clang-cache",
                    "XDG_CONFIG_HOME": scratch / "config",
                    "XDG_CACHE_HOME": scratch / "cache",
                }
            ),
        )


if __name__ == "__main__":
    raise SystemExit(host_contract_main(run, SUCCESS))
