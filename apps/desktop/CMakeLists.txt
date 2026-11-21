cmake_minimum_required(VERSION 3.24)
project(SkyberDesktop LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_MSVC_RUNTIME_LIBRARY MultiThreadedDLL)
find_package(Qt6 6.8.3 EXACT REQUIRED COMPONENTS Core Gui Qml Quick QmlImportScanner)
qt_standard_project_setup(REQUIRES 6.8)

include(FetchContent)
FetchContent_Declare(CxxQt
    GIT_REPOSITORY https://github.com/KDAB/cxx-qt-cmake.git
    GIT_TAG 64e63f560737b3b7f85ef8267c5918e4b8176d5a
)
FetchContent_MakeAvailable(CxxQt)
cxx_qt_import_crate(
    MANIFEST_PATH ../../Cargo.toml
    CRATES skyber_desktop_bridge
    FEATURES qt
    LOCKED
    QT_MODULES Qt6::Core
)

qt_add_executable(skyber-desktop main.cpp)
qt_add_qml_module(skyber-desktop
    URI Skyber
    QML_FILES Main.qml
)
target_link_libraries(skyber-desktop PRIVATE Qt6::Quick skyber_desktop_bridge)

enable_testing()
add_test(NAME skyber-desktop-smoke COMMAND skyber-desktop --smoke-test)
set_tests_properties(skyber-desktop-smoke PROPERTIES ENVIRONMENT "QT_QPA_PLATFORM=offscreen")
