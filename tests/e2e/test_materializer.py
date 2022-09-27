import sys
from pathlib import Path

from xplat.build_infra.buck_e2e.api.buck import Buck
from xplat.build_infra.buck_e2e.buck_workspace import buck_test, env


"""
If you need to add a directory that's isolated in buck2/test/targets
(ex. some test of form @buck_test(inplace=False, data_dir=some_new_directory)),
then you will need to update isolated_targets in buck2/test/targets/TARGETS.
Otherwise the test will fail because it cannot recognize the new directory.
"""

# Eden materializer only available on Linux
def eden_linux_only() -> bool:
    return sys.platform == "linux"


def watchman_dependency_linux_only() -> bool:
    return sys.platform == "linux"


def replace_in_file(old: str, new: str, file: Path, encoding: str = "utf-8") -> None:
    with open(file, encoding=encoding) as f:
        file_content = f.read()
    file_content = file_content.replace(old, new)
    with open(file, "w", encoding=encoding) as f:
        f.write(file_content)


@buck_test(inplace=False, data_dir="modify_deferred_materialization")
async def test_modify_input_source(buck: Buck) -> None:
    await buck.build("//:urandom_dep")

    targets_file = buck.cwd / "TARGETS.fixture"

    # Change the label in Targets.
    replace_in_file("__NOT_A_REAL_LABEL__", "buck2_test_local_exec", file=targets_file)

    await buck.build("//:urandom_dep")


@buck_test(
    inplace=False,
    data_dir="modify_deferred_materialization_deps",
    skip_if_windows=True,  # TODO(marwhal): Fix and enable on Windows
)
async def test_modify_dep_materialization(buck: Buck) -> None:
    await buck.build("//:check")

    with open(buck.cwd / "text", "w", encoding="utf-8") as f:
        f.write("TEXT2")

    await buck.build("//:check")


@buck_test(
    inplace=False, data_dir="deferred_materializer_matching_artifact_optimization"
)
@env("BUCK_LOG", "buck2_execute_impl::materializers=trace")
async def test_matching_artifact_optimization(buck: Buck) -> None:
    target = "root//:copy"
    result = await buck.build(target)
    # Check output is correctly materialized
    assert result.get_build_report().output_for_target(target).exists()

    # In this case, modifying `hidden` does not change the output, so the output should not
    # need to be rematerialized
    with open(buck.cwd / "hidden", "w", encoding="utf-8") as f:
        f.write("HIDDEN2")

    result = await buck.build(target)
    # Check output still exists
    assert result.get_build_report().output_for_target(target).exists()
    # Check that materializer did not report any rematerialization
    assert "already materialized, updating deps only" in result.stderr
    assert "materialize artifact" not in result.stderr

    # In this case, modifying `src` changes the output, so the output should be rematerialized
    with open(buck.cwd / "src", "w", encoding="utf-8") as f:
        f.write("SRC2")

    result = await buck.build(target)
    # Check output still exists
    output = result.get_build_report().output_for_target(target)
    assert output.exists()
    with open(output) as f:
        assert f.read().strip() == "SRC2"


@buck_test(
    inplace=False, data_dir="deferred_materializer_matching_artifact_optimization"
)
async def test_cache_directory_cleanup(buck: Buck) -> None:
    # sqlite materializer state is already enabled
    cache_dir = Path(buck.cwd, "buck-out", "v2", "cache")
    materializer_state_dir = cache_dir / "materializer_state"
    command_hashes_dir = cache_dir / "command_hashes"
    materializer_state_dir.mkdir(parents=True)
    command_hashes_dir.mkdir(parents=True)

    # Need to run a command to start the daemon.
    await buck.audit_config()

    cache_dir_listing = list(cache_dir.iterdir())
    assert cache_dir_listing == [materializer_state_dir]

    await buck.kill()
    disable_sqlite_materializer_state(buck)
    await buck.audit_config()

    cache_dir_listing = list(cache_dir.iterdir())
    assert cache_dir_listing == []


@buck_test(
    inplace=False, data_dir="deferred_materializer_matching_artifact_optimization"
)
@env("BUCK_LOG", "buck2_execute_impl::materializers=trace")
async def test_sqlite_materializer_state_matching_artifact_optimization(
    buck: Buck,
) -> None:
    # sqlite materializer state is already enabled
    target = "root//:copy"
    result = await buck.build(target)
    # Check output is correctly materialized
    assert result.get_build_report().output_for_target(target).exists()

    await buck.kill()

    result = await buck.build(target)
    # Check that materializer did not report any rematerialization
    assert "already materialized, updating deps only" in result.stderr, result.stderr
    assert "materialize artifact" not in result.stderr

    await buck.kill()

    # In this case, modifying `src` changes the output, so the output should be rematerialized
    with open(buck.cwd / "src", "w", encoding="utf-8") as f:
        f.write("SRC2")

    result = await buck.build(target)
    # Check output still exists
    output = result.get_build_report().output_for_target(target)
    assert output.exists()
    with open(output) as f:
        assert f.read().strip() == "SRC2"


@buck_test(
    inplace=False, data_dir="deferred_materializer_matching_artifact_optimization"
)
@env("BUCK_LOG", "buck2_execute_impl::materializers=trace")
async def test_download_file_sqlite_matching_artifact_optimization(
    buck: Buck,
) -> None:
    # sqlite materializer state is already enabled
    target = "root//:download"
    result = await buck.build(target)
    # Check output is correctly materialized
    assert result.get_build_report().output_for_target(target).exists()

    await buck.kill()

    result = await buck.build(target)
    # Check that materializer did not report any rematerialization
    assert "already materialized, updating deps only" in result.stderr, result.stderr
    assert "materialize artifact" not in result.stderr


@buck_test(
    inplace=False, data_dir="deferred_materializer_matching_artifact_optimization"
)
@env("BUCK_LOG", "buck2_execute_impl::materializers=trace")
async def test_sqlite_materializer_state_disabled(
    buck: Buck,
) -> None:
    disable_sqlite_materializer_state(buck)

    target = "root//:copy"
    result = await buck.build(target)
    # Check output is correctly materialized
    assert result.get_build_report().output_for_target(target).exists()

    await buck.kill()

    result = await buck.build(target)
    # Check that materializer did have to rematerialize the same artifact
    assert "already materialized, updating deps only" not in result.stderr
    assert "materialize artifact" in result.stderr


@buck_test(
    inplace=False, data_dir="deferred_materializer_matching_artifact_optimization"
)
@env("BUCK_LOG", "buck2_execute_impl::materializers=trace")
async def test_sqlite_materializer_state_buckconfig_version_change(
    buck: Buck,
) -> None:
    # sqlite materializer state is already enabled
    target = "root//:copy"
    result = await buck.build(target)
    # Check output is correctly materialized
    assert result.get_build_report().output_for_target(target).exists()

    await buck.kill()

    # Bump the buckconfig version of sqlite materializer state to invalidate the existing sqlite db
    replace_in_file(
        "sqlite_materializer_state_version = 0",
        "sqlite_materializer_state_version = 1",
        buck.cwd / ".buckconfig",
    )

    # just starting the buck2 daemon should delete the sqlite materializer state
    await buck.audit_config()


if eden_linux_only():

    @buck_test(inplace=False, data_dir="eden_materializer")
    async def test_eden_materialization_simple(buck: Buck) -> None:
        await buck.build("//:simple")


def set_materializer(buck: Buck, old: str, new: str) -> None:
    config_file = buck.cwd / ".buckconfig"

    # Change the label in Targets.
    old_config = "materializations = {}".format(old)
    new_config = "materializations = {}".format(new)
    replace_in_file(old_config, new_config, file=config_file)


def disable_sqlite_materializer_state(buck: Buck) -> None:
    config_file = buck.cwd / ".buckconfig"
    replace_in_file(
        "sqlite_materializer_state = true",
        "sqlite_materializer_state = false",
        file=config_file,
    )


if eden_linux_only():

    @buck_test(inplace=False, data_dir="eden_materializer")
    async def test_eden_materialization_clean_after_config_change(buck: Buck) -> None:
        set_materializer(buck, "eden", "deferred")
        await buck.build("//:simple")

        set_materializer(buck, "deferred", "eden")
        await buck.kill()
        await buck.build("//:simple")


if eden_linux_only():

    @buck_test(inplace=False, data_dir="eden_materializer")
    async def test_eden_materialization_no_config_change(buck: Buck) -> None:
        await buck.build("//:simple")
        await buck.kill()
        await buck.build("//:simple")
