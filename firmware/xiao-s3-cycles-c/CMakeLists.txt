# The C arm of `xiao-s3-cycles`: FreeRTOS as ESP-IDF ships it, on the same
# XIAO ESP32-S3, timed the same way. See README.md.
cmake_minimum_required(VERSION 3.16)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(xiao_s3_cycles_c)
