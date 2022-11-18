import pytest
from pybushka.async_ffi_client import RedisAsyncFFIClient
from pybushka.async_socket_client import RedisAsyncSocketClient
from pybushka.config import ClientConfiguration

default_host = "localhost"
default_port = 6379


def pytest_addoption(parser):
    parser.addoption(
        "--host",
        default=default_host,
        action="store",
        help="Redis host endpoint," " defaults to `%(default)s`",
    )

    parser.addoption(
        "--port",
        default=default_port,
        action="store",
        help="Redis port," " defaults to `%(default)s`",
    )


@pytest.fixture()
async def async_ffi_client(request):
    "Get async FFI client for tests"
    host = request.config.getoption("--host")
    port = request.config.getoption("--port")
    config = ClientConfiguration(host=host, port=port)
    return await RedisAsyncFFIClient.create(config)


@pytest.fixture()
async def async_socket_client(request):
    "Get async socket client for tests"
    host = request.config.getoption("--host")
    port = request.config.getoption("--port")
    config = ClientConfiguration(host=host, port=port)
    return await RedisAsyncSocketClient.create(config)