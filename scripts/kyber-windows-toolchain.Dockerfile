FROM registry.gitlab.com/kyber/ops/docker-images/debian-win64@sha256:39632e7b0283d5686cfae181b0c85d17b68e3a3024d4054c5eae64b80d8c9694

RUN uv tool install --python /usr/bin/python3 \
    --with 'jinja2==3.1.6' \
    --with 'markupsafe==3.0.3' \
    'meson==1.12.1'

ENV PATH="/root/.local/bin:/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
ENV CARGO_HOME=/tmp/cargo
ENV MESON_PACKAGE_CACHE_DIR=/tmp/meson-packagecache
