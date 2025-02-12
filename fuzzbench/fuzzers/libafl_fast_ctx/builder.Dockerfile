# Copyright 2020 Google LLC
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

ARG parent_image
FROM $parent_image

# Install libstdc++ to use llvm_mode.
RUN apt-get update && \
    apt-get install -y wget libstdc++-5-dev libtool-bin automake flex bison \
                       libglib2.0-dev libpixman-1-dev python3-setuptools unzip \
                       apt-utils apt-transport-https ca-certificates joe curl \
                       python3-dev gzip

# Uninstall old Rust
RUN if which rustup; then rustup self uninstall -y; fi

# Install latest Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs > /rustup.sh && \
    sh /rustup.sh -y

# Download libafl
RUN git clone <ANONYMIZED> /libafl_fuzzbench && \
    cd /libafl_fuzzbench && \
    git checkout 9097911fe37526e36ee25a9f958deb924cc7032d && \
    git submodule update --init

# Compile libafl
RUN cd /libafl_fuzzbench/ && unset CFLAGS && unset CXXFLAGS && \
    export CC=clang && export CXX=clang++ && \
    PATH="$PATH:/root/.cargo/bin/" cargo build --release

RUN wget <ANONYMIZED>empty_fuzzer_lib.c -O /empty_fuzzer_lib.c && \
    clang -c /empty_fuzzer_lib.c && \
    ar r /emptylib.a *.o
