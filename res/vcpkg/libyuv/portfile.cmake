vcpkg_check_linkage(ONLY_STATIC_LIBRARY)

vcpkg_from_git(
    OUT_SOURCE_PATH SOURCE_PATH
    URL https://chromium.googlesource.com/libyuv/libyuv
    REF 0faf8dd0e004520a61a603a4d2996d5ecc80dc3f
    # Check https://chromium.googlesource.com/libyuv/libyuv/+/refs/heads/main/include/libyuv/version.h for a version!
    PATCHES
        fix-cmakelists.patch
)

vcpkg_cmake_get_vars(cmake_vars_file)
include("${cmake_vars_file}")
if (VCPKG_DETECTED_CMAKE_CXX_COMPILER_ID STREQUAL "MSVC" AND NOT VCPKG_TARGET_IS_UWP)
    # Most of libyuv accelerated features need to be compiled by clang/gcc, so force use clang-cl, otherwise the performance is too poor.
    # Manually build the port with clang-cl when using MSVC as compiler
    #
    # 只在 PATH 上已经有 clang-cl 时才用它。原先这里是 vcpkg_find_acquire_program(CLANG),
    # 那会去下载 LLVM 的安装包 —— 离线环境(内网构建机)下必然失败, 而这个 port 本身
    # 是仓库自带的 overlay port, 不是上游 vcpkg 的, 改它只影响本仓库的构建。
    # 找不到 clang-cl 时退回 MSVC: 库一样能编出来, 只是慢一些(官方也这么说明,
    # 见同目录下的 usage-msvc)。
    find_program(CLANG_ON_PATH NAMES clang-cl.exe clang-cl)
    if(CLANG_ON_PATH)
        set(CLANG "${CLANG_ON_PATH}")
    else()
        set(CLANG "-NOTFOUND")
    endif()

    if (CLANG MATCHES "-NOTFOUND")
        message(STATUS "No clang-cl on PATH, building libyuv with MSVC instead (slower).")
    else()
        message(STATUS "Set compiler to clang-cl when using MSVC")

        # https://github.com/microsoft/vcpkg/pull/10398
        set(VCPKG_POLICY_SKIP_ARCHITECTURE_CHECK enabled)

        get_filename_component(CLANG "${CLANG}" DIRECTORY)

        if(VCPKG_TARGET_ARCHITECTURE STREQUAL "arm")
            set(CLANG_TARGET "arm")
        elseif(VCPKG_TARGET_ARCHITECTURE STREQUAL "arm64")
            set(CLANG_TARGET "aarch64")
        elseif(VCPKG_TARGET_ARCHITECTURE STREQUAL "x86")
            set(CLANG_TARGET "i686")
        elseif(VCPKG_TARGET_ARCHITECTURE STREQUAL "x64")
            set(CLANG_TARGET "x86_64")
        else()
            message(FATAL_ERROR "Unsupported target architecture")
        endif()

        set(CLANG_TARGET "${CLANG_TARGET}-pc-windows-msvc")

        message(STATUS "Using clang target ${CLANG_TARGET}")
        string(APPEND VCPKG_DETECTED_CMAKE_CXX_FLAGS --target=${CLANG_TARGET})
        string(APPEND VCPKG_DETECTED_CMAKE_C_FLAGS --target=${CLANG_TARGET})

        set(BUILD_OPTIONS
                -DCMAKE_CXX_COMPILER=${CLANG}/clang-cl.exe
                -DCMAKE_C_COMPILER=${CLANG}/clang-cl.exe
                -DCMAKE_CXX_FLAGS=${VCPKG_DETECTED_CMAKE_CXX_FLAGS}
                -DCMAKE_C_FLAGS=${VCPKG_DETECTED_CMAKE_C_FLAGS})
    endif ()
endif ()

vcpkg_cmake_configure(
    SOURCE_PATH ${SOURCE_PATH}
    DISABLE_PARALLEL_CONFIGURE
    OPTIONS
        ${BUILD_OPTIONS}
    OPTIONS_DEBUG
        -DCMAKE_DEBUG_POSTFIX=d
)

vcpkg_cmake_install()
vcpkg_copy_pdbs()

vcpkg_cmake_config_fixup(CONFIG_PATH share/cmake/libyuv)

file(REMOVE_RECURSE ${CURRENT_PACKAGES_DIR}/debug/include)
file(REMOVE_RECURSE ${CURRENT_PACKAGES_DIR}/debug/share)

configure_file(${CMAKE_CURRENT_LIST_DIR}/libyuv-config.cmake ${CURRENT_PACKAGES_DIR}/share/${PORT} COPYONLY)
file(INSTALL ${SOURCE_PATH}/LICENSE DESTINATION ${CURRENT_PACKAGES_DIR}/share/${PORT} RENAME copyright)

vcpkg_cmake_get_vars(cmake_vars_file)
include("${cmake_vars_file}")
if (VCPKG_DETECTED_CMAKE_CXX_COMPILER_ID STREQUAL "MSVC")
    message(WARNING "Use MSVC to compile libyuv results in a very slow library. (https://github.com/microsoft/vcpkg/issues/28446)")
    file(INSTALL "${CMAKE_CURRENT_LIST_DIR}/usage-msvc" DESTINATION "${CURRENT_PACKAGES_DIR}/share/${PORT}" RENAME "usage")
else ()
    file(INSTALL "${CMAKE_CURRENT_LIST_DIR}/usage" DESTINATION "${CURRENT_PACKAGES_DIR}/share/${PORT}")
endif ()
