"""Fluent programmable runtime client for local A3S Box."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Literal, TypeVar, cast

from .models import (
    BuildImageInfo,
    ImageHealthCheckInfo,
    ImageHistoryInfo,
    ImageInfo,
    ImageInspectInfo,
    NetworkEndpointInfo,
    NetworkInfo,
    PortMapping,
    PushImageInfo,
    RegistryCredentials,
    SandboxNetwork,
    SdkCapabilities,
    SignaturePolicy,
    TmpfsMount,
    VolumeInfo,
    VolumeMount,
)
from .runtime import (
    A3SAsyncLocalRuntime,
    A3SLocalRuntime,
    AsyncLocalRuntime,
    LocalRuntime,
)
from .sandbox import DEFAULT_IMAGE, AsyncSandbox, Sandbox

LiteralIsolation = Literal["microvm", "sandbox"]
RegistryProtocol = Literal["https", "http"]


class A3SBoxClient:
    """Synchronous resource client with fluent terminal builders."""

    def __init__(self, runtime: LocalRuntime | None = None) -> None:
        self._runtime = runtime or A3SLocalRuntime()

    def image(self, context_dir: str) -> ImageBuilder:
        return ImageBuilder(self._runtime, context_dir)

    def volume(self, name: str) -> VolumeBuilder:
        return VolumeBuilder(self._runtime, name)

    def network(self, name: str) -> NetworkBuilder:
        return NetworkBuilder(self._runtime, name)

    def sandbox(self, image: str = DEFAULT_IMAGE) -> SandboxBuilder:
        return SandboxBuilder(self._runtime, image)

    def pull_image(
        self,
        reference: str,
        *,
        force: bool = False,
        platform: str | None = None,
        credentials: RegistryCredentials | None = None,
        signature_policy: SignaturePolicy | None = None,
    ) -> ImageInfo:
        result = self._runtime.request(
            {
                "operation": "image_pull",
                "reference": reference,
                "force": force,
                **({} if platform is None else {"platform": platform}),
                **(
                    {}
                    if credentials is None
                    else {"credentials": credentials.bridge_value()}
                ),
                **(
                    {}
                    if signature_policy is None
                    else {"signature_policy": signature_policy.bridge_value()}
                ),
            }
        )
        return _image_info(result)

    def get_image(self, reference: str) -> ImageInfo | None:
        result = self._runtime.request(
            {"operation": "image_get", "reference": reference}
        )
        value = result.get("image")
        return None if value is None else _image_info(_mapping(value))

    def list_images(self) -> list[ImageInfo]:
        result = self._runtime.request({"operation": "image_list"})
        return [_image_info(item) for item in _mapping_list(result, "images")]

    def inspect_image(self, reference: str) -> ImageInspectInfo | None:
        result = self._runtime.request(
            {"operation": "image_inspect", "reference": reference}
        )
        value = result.get("image")
        return None if value is None else _image_inspect_info(_mapping(value))

    def image_history(self, reference: str) -> list[ImageHistoryInfo] | None:
        result = self._runtime.request(
            {"operation": "image_history", "reference": reference}
        )
        value = result.get("history")
        return (
            None
            if value is None
            else [
                _image_history_info(item)
                for item in _mapping_sequence(value)
            ]
        )

    def tag_image(self, source: str, target: str) -> ImageInfo:
        return _image_info(
            self._runtime.request(
                {
                    "operation": "image_tag",
                    "source": source,
                    "target": target,
                }
            )
        )

    def push_image(
        self,
        source: str,
        target: str,
        *,
        credentials: RegistryCredentials | None = None,
        protocol: RegistryProtocol | None = None,
    ) -> PushImageInfo:
        return _push_image_info(
            self._runtime.request(
                {
                    "operation": "image_push",
                    "source": source,
                    "target": target,
                    **(
                        {}
                        if credentials is None
                        else {"credentials": credentials.bridge_value()}
                    ),
                    **(
                        {}
                        if protocol is None
                        else {"registry_protocol": protocol}
                    ),
                }
            )
        )

    def remove_image(self, reference: str) -> None:
        self._runtime.request(
            {"operation": "image_remove", "reference": reference}
        )

    def evict_images(self) -> list[str]:
        result = self._runtime.request({"operation": "image_evict"})
        return [str(value) for value in _sequence(result["references"])]

    def get_volume(self, name: str) -> VolumeInfo | None:
        result = self._runtime.request({"operation": "volume_get", "name": name})
        value = result.get("volume")
        return None if value is None else _volume_info(_mapping(value))

    def list_volumes(self) -> list[VolumeInfo]:
        result = self._runtime.request({"operation": "volume_list"})
        return [_volume_info(item) for item in _mapping_list(result, "volumes")]

    def remove_volume(self, name: str, *, force: bool = False) -> VolumeInfo:
        return _volume_info(
            self._runtime.request(
                {
                    "operation": "volume_remove",
                    "name": name,
                    "force": force,
                }
            )
        )

    def prune_volumes(self) -> list[str]:
        result = self._runtime.request({"operation": "volume_prune"})
        return [str(value) for value in _sequence(result["names"])]

    def get_network(self, name: str) -> NetworkInfo | None:
        result = self._runtime.request({"operation": "network_get", "name": name})
        value = result.get("network")
        return None if value is None else _network_info(_mapping(value))

    def list_networks(self) -> list[NetworkInfo]:
        result = self._runtime.request({"operation": "network_list"})
        return [_network_info(item) for item in _mapping_list(result, "networks")]

    def remove_network(self, name: str) -> NetworkInfo:
        return _network_info(
            self._runtime.request({"operation": "network_remove", "name": name})
        )

    def prune_networks(self) -> list[str]:
        result = self._runtime.request({"operation": "network_prune"})
        return [str(value) for value in _sequence(result["names"])]

    def capabilities(self) -> SdkCapabilities:
        return _sdk_capabilities(
            self._runtime.request({"operation": "sdk_capabilities"})
        )


class A3SAsyncBoxClient:
    """Asynchronous counterpart of :class:`A3SBoxClient`."""

    def __init__(self, runtime: AsyncLocalRuntime | None = None) -> None:
        self._runtime = runtime or A3SAsyncLocalRuntime()

    def image(self, context_dir: str) -> AsyncImageBuilder:
        return AsyncImageBuilder(self._runtime, context_dir)

    def volume(self, name: str) -> AsyncVolumeBuilder:
        return AsyncVolumeBuilder(self._runtime, name)

    def network(self, name: str) -> AsyncNetworkBuilder:
        return AsyncNetworkBuilder(self._runtime, name)

    def sandbox(self, image: str = DEFAULT_IMAGE) -> AsyncSandboxBuilder:
        return AsyncSandboxBuilder(self._runtime, image)

    async def pull_image(
        self,
        reference: str,
        *,
        force: bool = False,
        platform: str | None = None,
        credentials: RegistryCredentials | None = None,
        signature_policy: SignaturePolicy | None = None,
    ) -> ImageInfo:
        result = await self._runtime.request(
            {
                "operation": "image_pull",
                "reference": reference,
                "force": force,
                **({} if platform is None else {"platform": platform}),
                **(
                    {}
                    if credentials is None
                    else {"credentials": credentials.bridge_value()}
                ),
                **(
                    {}
                    if signature_policy is None
                    else {"signature_policy": signature_policy.bridge_value()}
                ),
            }
        )
        return _image_info(result)

    async def get_image(self, reference: str) -> ImageInfo | None:
        result = await self._runtime.request(
            {"operation": "image_get", "reference": reference}
        )
        value = result.get("image")
        return None if value is None else _image_info(_mapping(value))

    async def list_images(self) -> list[ImageInfo]:
        result = await self._runtime.request({"operation": "image_list"})
        return [_image_info(item) for item in _mapping_list(result, "images")]

    async def inspect_image(self, reference: str) -> ImageInspectInfo | None:
        result = await self._runtime.request(
            {"operation": "image_inspect", "reference": reference}
        )
        value = result.get("image")
        return None if value is None else _image_inspect_info(_mapping(value))

    async def image_history(
        self,
        reference: str,
    ) -> list[ImageHistoryInfo] | None:
        result = await self._runtime.request(
            {"operation": "image_history", "reference": reference}
        )
        value = result.get("history")
        return (
            None
            if value is None
            else [
                _image_history_info(item)
                for item in _mapping_sequence(value)
            ]
        )

    async def tag_image(self, source: str, target: str) -> ImageInfo:
        return _image_info(
            await self._runtime.request(
                {
                    "operation": "image_tag",
                    "source": source,
                    "target": target,
                }
            )
        )

    async def push_image(
        self,
        source: str,
        target: str,
        *,
        credentials: RegistryCredentials | None = None,
        protocol: RegistryProtocol | None = None,
    ) -> PushImageInfo:
        return _push_image_info(
            await self._runtime.request(
                {
                    "operation": "image_push",
                    "source": source,
                    "target": target,
                    **(
                        {}
                        if credentials is None
                        else {"credentials": credentials.bridge_value()}
                    ),
                    **(
                        {}
                        if protocol is None
                        else {"registry_protocol": protocol}
                    ),
                }
            )
        )

    async def remove_image(self, reference: str) -> None:
        await self._runtime.request(
            {"operation": "image_remove", "reference": reference}
        )

    async def evict_images(self) -> list[str]:
        result = await self._runtime.request({"operation": "image_evict"})
        return [str(value) for value in _sequence(result["references"])]

    async def get_volume(self, name: str) -> VolumeInfo | None:
        result = await self._runtime.request({"operation": "volume_get", "name": name})
        value = result.get("volume")
        return None if value is None else _volume_info(_mapping(value))

    async def list_volumes(self) -> list[VolumeInfo]:
        result = await self._runtime.request({"operation": "volume_list"})
        return [_volume_info(item) for item in _mapping_list(result, "volumes")]

    async def remove_volume(self, name: str, *, force: bool = False) -> VolumeInfo:
        return _volume_info(
            await self._runtime.request(
                {
                    "operation": "volume_remove",
                    "name": name,
                    "force": force,
                }
            )
        )

    async def prune_volumes(self) -> list[str]:
        result = await self._runtime.request({"operation": "volume_prune"})
        return [str(value) for value in _sequence(result["names"])]

    async def get_network(self, name: str) -> NetworkInfo | None:
        result = await self._runtime.request({"operation": "network_get", "name": name})
        value = result.get("network")
        return None if value is None else _network_info(_mapping(value))

    async def list_networks(self) -> list[NetworkInfo]:
        result = await self._runtime.request({"operation": "network_list"})
        return [_network_info(item) for item in _mapping_list(result, "networks")]

    async def remove_network(self, name: str) -> NetworkInfo:
        return _network_info(
            await self._runtime.request({"operation": "network_remove", "name": name})
        )

    async def prune_networks(self) -> list[str]:
        result = await self._runtime.request({"operation": "network_prune"})
        return [str(value) for value in _sequence(result["names"])]

    async def capabilities(self) -> SdkCapabilities:
        return _sdk_capabilities(
            await self._runtime.request({"operation": "sdk_capabilities"})
        )


_ImageBuilderT = TypeVar("_ImageBuilderT", bound="_ImageBuilderBase")


class _ImageBuilderBase:
    def __init__(self, context_dir: str) -> None:
        self._context_dir = context_dir
        self._dockerfile: str | None = None
        self._tag: str | None = None
        self._build_args: dict[str, str] = {}
        self._quiet = True
        self._platforms: list[str] = []
        self._target: str | None = None
        self._no_cache = False

    def dockerfile(self: _ImageBuilderT, path: str) -> _ImageBuilderT:
        self._dockerfile = path
        return self

    def tag(self: _ImageBuilderT, tag: str) -> _ImageBuilderT:
        self._tag = tag
        return self

    def build_arg(
        self: _ImageBuilderT,
        key: str,
        value: str,
    ) -> _ImageBuilderT:
        self._build_args[key] = value
        return self

    def quiet(self: _ImageBuilderT, enabled: bool = True) -> _ImageBuilderT:
        self._quiet = enabled
        return self

    def platform(self: _ImageBuilderT, platform: str) -> _ImageBuilderT:
        self._platforms.append(platform)
        return self

    def target(self: _ImageBuilderT, target: str) -> _ImageBuilderT:
        self._target = target
        return self

    def no_cache(self: _ImageBuilderT, enabled: bool = True) -> _ImageBuilderT:
        self._no_cache = enabled
        return self

    def _request(self) -> dict[str, object]:
        return {
            "operation": "image_build",
            "context_dir": self._context_dir,
            "build_args": dict(self._build_args),
            "quiet": self._quiet,
            "platforms": list(self._platforms),
            "no_cache": self._no_cache,
            **({} if self._dockerfile is None else {"dockerfile": self._dockerfile}),
            **({} if self._tag is None else {"tag": self._tag}),
            **({} if self._target is None else {"target": self._target}),
        }


class ImageBuilder(_ImageBuilderBase):
    def __init__(self, runtime: LocalRuntime, context_dir: str) -> None:
        super().__init__(context_dir)
        self._runtime = runtime

    def build(self) -> BuildImageInfo:
        return _build_image_info(self._runtime.request(self._request()))


class AsyncImageBuilder(_ImageBuilderBase):
    def __init__(self, runtime: AsyncLocalRuntime, context_dir: str) -> None:
        super().__init__(context_dir)
        self._runtime = runtime

    async def build(self) -> BuildImageInfo:
        return _build_image_info(await self._runtime.request(self._request()))


_VolumeBuilderT = TypeVar("_VolumeBuilderT", bound="_VolumeBuilderBase")


class _VolumeBuilderBase:
    def __init__(self, name: str) -> None:
        self._name = name
        self._labels: dict[str, str] = {}
        self._size_limit = 0

    def label(
        self: _VolumeBuilderT,
        key: str,
        value: str,
    ) -> _VolumeBuilderT:
        self._labels[key] = value
        return self

    def size_limit(self: _VolumeBuilderT, size_bytes: int) -> _VolumeBuilderT:
        self._size_limit = size_bytes
        return self

    def _request(self) -> dict[str, object]:
        return {
            "operation": "volume_create",
            "name": self._name,
            "labels": dict(self._labels),
            "size_limit": self._size_limit,
        }


class VolumeBuilder(_VolumeBuilderBase):
    def __init__(self, runtime: LocalRuntime, name: str) -> None:
        super().__init__(name)
        self._runtime = runtime

    def create(self) -> VolumeInfo:
        return _volume_info(self._runtime.request(self._request()))


class AsyncVolumeBuilder(_VolumeBuilderBase):
    def __init__(self, runtime: AsyncLocalRuntime, name: str) -> None:
        super().__init__(name)
        self._runtime = runtime

    async def create(self) -> VolumeInfo:
        return _volume_info(await self._runtime.request(self._request()))


_NetworkBuilderT = TypeVar("_NetworkBuilderT", bound="_NetworkBuilderBase")


class _NetworkBuilderBase:
    def __init__(self, name: str) -> None:
        self._name = name
        self._subnet = "10.89.0.0/24"
        self._labels: dict[str, str] = {}

    def subnet(self: _NetworkBuilderT, subnet: str) -> _NetworkBuilderT:
        self._subnet = subnet
        return self

    def label(
        self: _NetworkBuilderT,
        key: str,
        value: str,
    ) -> _NetworkBuilderT:
        self._labels[key] = value
        return self

    def _request(self) -> dict[str, object]:
        return {
            "operation": "network_create",
            "name": self._name,
            "subnet": self._subnet,
            "labels": dict(self._labels),
        }


class NetworkBuilder(_NetworkBuilderBase):
    def __init__(self, runtime: LocalRuntime, name: str) -> None:
        super().__init__(name)
        self._runtime = runtime

    def create(self) -> NetworkInfo:
        return _network_info(self._runtime.request(self._request()))


class AsyncNetworkBuilder(_NetworkBuilderBase):
    def __init__(self, runtime: AsyncLocalRuntime, name: str) -> None:
        super().__init__(name)
        self._runtime = runtime

    async def create(self) -> NetworkInfo:
        return _network_info(await self._runtime.request(self._request()))


_SandboxBuilderT = TypeVar("_SandboxBuilderT", bound="_SandboxBuilderBase")


class _SandboxBuilderBase:
    def __init__(self, image: str) -> None:
        self._image = image
        self._timeout = 3600
        self._envs: dict[str, str] = {}
        self._metadata: dict[str, str] = {}
        self._name: str | None = None
        self._cpus: int | None = None
        self._memory_mb: int | None = None
        self._isolation: LiteralIsolation = "microvm"
        self._snapshot: str | None = None
        self._workspace: str | None = None
        self._workdir: str | None = None
        self._user: str | None = None
        self._hostname: str | None = None
        self._mounts: list[VolumeMount] = []
        self._tmpfs: list[TmpfsMount] = []
        self._network = SandboxNetwork.tsi()
        self._ports: list[PortMapping] = []
        self._dns: list[str] = []
        self._host_aliases: dict[str, str] = {}
        self._read_only = False
        self._persistent = False
        self._auto_remove = True

    def timeout(self: _SandboxBuilderT, seconds: int) -> _SandboxBuilderT:
        self._timeout = seconds
        return self

    def env(
        self: _SandboxBuilderT,
        key: str,
        value: str,
    ) -> _SandboxBuilderT:
        self._envs[key] = value
        return self

    def metadata(
        self: _SandboxBuilderT,
        key: str,
        value: str,
    ) -> _SandboxBuilderT:
        self._metadata[key] = value
        return self

    def name(self: _SandboxBuilderT, name: str) -> _SandboxBuilderT:
        self._name = name
        return self

    def cpus(self: _SandboxBuilderT, cpus: int) -> _SandboxBuilderT:
        self._cpus = cpus
        return self

    def memory_mb(self: _SandboxBuilderT, memory_mb: int) -> _SandboxBuilderT:
        self._memory_mb = memory_mb
        return self

    def isolation(
        self: _SandboxBuilderT,
        isolation: LiteralIsolation,
    ) -> _SandboxBuilderT:
        self._isolation = isolation
        return self

    def filesystem_snapshot(
        self: _SandboxBuilderT,
        snapshot_id: str,
    ) -> _SandboxBuilderT:
        self._snapshot = snapshot_id
        return self

    def workspace(self: _SandboxBuilderT, path: str) -> _SandboxBuilderT:
        self._workspace = path
        return self

    def workdir(self: _SandboxBuilderT, path: str) -> _SandboxBuilderT:
        self._workdir = path
        return self

    def user(self: _SandboxBuilderT, user: str) -> _SandboxBuilderT:
        self._user = user
        return self

    def hostname(self: _SandboxBuilderT, hostname: str) -> _SandboxBuilderT:
        self._hostname = hostname
        return self

    def mount(
        self: _SandboxBuilderT,
        mount: VolumeMount,
    ) -> _SandboxBuilderT:
        self._mounts.append(mount)
        return self

    def mount_bind(
        self: _SandboxBuilderT,
        source: str,
        target: str,
        *,
        read_only: bool = False,
    ) -> _SandboxBuilderT:
        return self.mount(VolumeMount.bind(source, target, read_only=read_only))

    def mount_named(
        self: _SandboxBuilderT,
        name: str,
        target: str,
        *,
        read_only: bool = False,
    ) -> _SandboxBuilderT:
        return self.mount(VolumeMount.named(name, target, read_only=read_only))

    def tmpfs(
        self: _SandboxBuilderT,
        target: str,
        *,
        size_bytes: int | None = None,
        read_only: bool = False,
    ) -> _SandboxBuilderT:
        self._tmpfs.append(TmpfsMount(target, size_bytes, read_only))
        return self

    def network(
        self: _SandboxBuilderT,
        network: str | SandboxNetwork,
    ) -> _SandboxBuilderT:
        self._network = (
            SandboxNetwork.bridge(network)
            if isinstance(network, str)
            else network
        )
        return self

    def disable_network(self: _SandboxBuilderT) -> _SandboxBuilderT:
        self._network = SandboxNetwork.disabled()
        return self

    def publish_tcp(
        self: _SandboxBuilderT,
        host_port: int,
        guest_port: int,
    ) -> _SandboxBuilderT:
        self._ports.append(PortMapping.tcp(host_port, guest_port))
        return self

    def dns_server(self: _SandboxBuilderT, address: str) -> _SandboxBuilderT:
        self._dns.append(address)
        return self

    def host_alias(
        self: _SandboxBuilderT,
        host: str,
        address: str,
    ) -> _SandboxBuilderT:
        self._host_aliases[host] = address
        return self

    def read_only(
        self: _SandboxBuilderT,
        enabled: bool = True,
    ) -> _SandboxBuilderT:
        self._read_only = enabled
        return self

    def persistent(
        self: _SandboxBuilderT,
        enabled: bool = True,
    ) -> _SandboxBuilderT:
        self._persistent = enabled
        return self

    def auto_remove(
        self: _SandboxBuilderT,
        enabled: bool = True,
    ) -> _SandboxBuilderT:
        self._auto_remove = enabled
        return self


class SandboxBuilder(_SandboxBuilderBase):
    def __init__(self, runtime: LocalRuntime, image: str) -> None:
        super().__init__(image)
        self._runtime = runtime

    def start(self) -> Sandbox:
        return Sandbox.create(
            self._image,
            timeout=self._timeout,
            envs=self._envs,
            metadata=self._metadata,
            name=self._name,
            cpus=self._cpus,
            memory_mb=self._memory_mb,
            isolation=self._isolation,
            filesystem_snapshot_id=self._snapshot,
            workspace=self._workspace,
            workdir=self._workdir,
            user=self._user,
            hostname=self._hostname,
            mounts=self._mounts,
            tmpfs=self._tmpfs,
            network=self._network,
            ports=self._ports,
            dns=self._dns,
            host_aliases=self._host_aliases,
            read_only=self._read_only,
            persistent=self._persistent,
            auto_remove=self._auto_remove,
            runtime=self._runtime,
        )


class AsyncSandboxBuilder(_SandboxBuilderBase):
    def __init__(self, runtime: AsyncLocalRuntime, image: str) -> None:
        super().__init__(image)
        self._runtime = runtime

    async def start(self) -> AsyncSandbox:
        return await AsyncSandbox.create(
            self._image,
            timeout=self._timeout,
            envs=self._envs,
            metadata=self._metadata,
            name=self._name,
            cpus=self._cpus,
            memory_mb=self._memory_mb,
            isolation=self._isolation,
            filesystem_snapshot_id=self._snapshot,
            workspace=self._workspace,
            workdir=self._workdir,
            user=self._user,
            hostname=self._hostname,
            mounts=self._mounts,
            tmpfs=self._tmpfs,
            network=self._network,
            ports=self._ports,
            dns=self._dns,
            host_aliases=self._host_aliases,
            read_only=self._read_only,
            persistent=self._persistent,
            auto_remove=self._auto_remove,
            runtime=self._runtime,
        )


def _build_image_info(result: Mapping[str, object]) -> BuildImageInfo:
    return BuildImageInfo(
        reference=str(result["reference"]),
        digest=str(result["digest"]),
        size_bytes=int(cast(int, result["size_bytes"])),
        layer_count=int(cast(int, result["layer_count"])),
    )


def _image_info(result: Mapping[str, object]) -> ImageInfo:
    return ImageInfo(
        reference=str(result["reference"]),
        digest=str(result["digest"]),
        size_bytes=int(cast(int, result["size_bytes"])),
        pulled_at=str(result["pulled_at"]),
        last_used=str(result["last_used"]),
        path=str(result["path"]),
    )


def _image_inspect_info(result: Mapping[str, object]) -> ImageInspectInfo:
    health_value = result.get("health_check")
    return ImageInspectInfo(
        reference=str(result["reference"]),
        digest=str(result["digest"]),
        size_bytes=int(cast(int, result["size_bytes"])),
        pulled_at=str(result["pulled_at"]),
        last_used=str(result["last_used"]),
        path=str(result["path"]),
        manifest_digest=str(result["manifest_digest"]),
        layer_count=int(cast(int, result["layer_count"])),
        entrypoint=_optional_string_tuple(result.get("entrypoint")),
        command=_optional_string_tuple(result.get("command")),
        env=_string_mapping(result["env"]),
        working_dir=_optional_string(result.get("working_dir")),
        user=_optional_string(result.get("user")),
        exposed_ports=tuple(
            str(value) for value in _sequence(result["exposed_ports"])
        ),
        volumes=tuple(str(value) for value in _sequence(result["volumes"])),
        stop_signal=_optional_string(result.get("stop_signal")),
        health_check=(
            None
            if health_value is None
            else _image_health_check_info(_mapping(health_value))
        ),
        onbuild=tuple(str(value) for value in _sequence(result["onbuild"])),
        labels=_string_mapping(result["labels"]),
    )


def _image_health_check_info(
    result: Mapping[str, object],
) -> ImageHealthCheckInfo:
    return ImageHealthCheckInfo(
        test=tuple(str(value) for value in _sequence(result["test"])),
        interval=_optional_int(result.get("interval")),
        timeout=_optional_int(result.get("timeout")),
        retries=_optional_int(result.get("retries")),
        start_period=_optional_int(result.get("start_period")),
    )


def _image_history_info(result: Mapping[str, object]) -> ImageHistoryInfo:
    return ImageHistoryInfo(
        created=_optional_string(result.get("created")),
        created_by=str(result["created_by"]),
        size_bytes=int(cast(int, result["size_bytes"])),
        comment=str(result["comment"]),
        empty_layer=bool(result["empty_layer"]),
    )


def _push_image_info(result: Mapping[str, object]) -> PushImageInfo:
    return PushImageInfo(
        reference=str(result["reference"]),
        manifest_digest=str(result["manifest_digest"]),
        config_url=str(result["config_url"]),
        manifest_url=str(result["manifest_url"]),
    )


def _sdk_capabilities(result: Mapping[str, object]) -> SdkCapabilities:
    return SdkCapabilities(
        protocol_version=int(cast(int, result["protocol_version"])),
        operations=tuple(str(value) for value in _sequence(result["operations"])),
    )


def _volume_info(result: Mapping[str, object]) -> VolumeInfo:
    return VolumeInfo(
        name=str(result["name"]),
        driver=str(result["driver"]),
        mount_point=str(result["mount_point"]),
        labels=_string_mapping(result["labels"]),
        in_use_by=tuple(str(value) for value in _sequence(result["in_use_by"])),
        in_use=bool(result["in_use"]),
        size_limit=int(cast(int, result["size_limit"])),
        created_at=str(result["created_at"]),
    )


def _network_info(result: Mapping[str, object]) -> NetworkInfo:
    return NetworkInfo(
        name=str(result["name"]),
        driver=str(result["driver"]),
        subnet=str(result["subnet"]),
        gateway=str(result["gateway"]),
        labels=_string_mapping(result["labels"]),
        endpoints=tuple(
            _network_endpoint(item)
            for item in _mapping_sequence(result["endpoints"])
        ),
        endpoint_count=int(cast(int, result["endpoint_count"])),
        isolation=str(result["isolation"]),
        created_at=str(result["created_at"]),
    )


def _network_endpoint(result: Mapping[str, object]) -> NetworkEndpointInfo:
    return NetworkEndpointInfo(
        box_id=str(result["box_id"]),
        box_name=str(result["box_name"]),
        aliases=tuple(str(value) for value in _sequence(result["aliases"])),
        ip_address=str(result["ip_address"]),
        mac_address=str(result["mac_address"]),
    )


def _mapping(value: object) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise TypeError("A3S Box bridge returned a non-object value")
    return cast(Mapping[str, object], value)


def _sequence(value: object) -> Sequence[object]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes)):
        raise TypeError("A3S Box bridge returned a non-array value")
    return cast(Sequence[object], value)


def _mapping_sequence(value: object) -> list[Mapping[str, object]]:
    return [_mapping(item) for item in _sequence(value)]


def _mapping_list(
    result: Mapping[str, object],
    key: str,
) -> list[Mapping[str, object]]:
    return _mapping_sequence(result[key])


def _string_mapping(value: object) -> dict[str, str]:
    return {str(key): str(item) for key, item in _mapping(value).items()}


def _optional_string(value: object) -> str | None:
    return None if value is None else str(value)


def _optional_string_tuple(value: object) -> tuple[str, ...] | None:
    return (
        None
        if value is None
        else tuple(str(item) for item in _sequence(value))
    )


def _optional_int(value: object) -> int | None:
    return None if value is None else int(cast(int, value))
