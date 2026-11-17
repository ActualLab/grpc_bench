#!/usr/bin/env bash

## The list of benchmarks to run
BENCHMARKS_TO_RUN="${@}"
##  ...or use all the *_bench dirs by default
BENCHMARKS_TO_RUN="${BENCHMARKS_TO_RUN:-$(find . -maxdepth 1 -name '*_bench' -type d | sort)}"

RESULTS_DIR="results/$(date '+%y%m%dT%H%M%S')"
export GRPC_BENCHMARK_DURATION=${GRPC_BENCHMARK_DURATION:-"20s"}
export GRPC_BENCHMARK_WARMUP=${GRPC_BENCHMARK_WARMUP:-"5s"}
export GRPC_SERVER_CPUS=${GRPC_SERVER_CPUS:-"1"}
export GRPC_SERVER_RAM=${GRPC_SERVER_RAM:-"512m"}
export GRPC_CLIENT_CONNECTIONS=${GRPC_CLIENT_CONNECTIONS:-"50"}
export GRPC_CLIENT_CONCURRENCY=${GRPC_CLIENT_CONCURRENCY:-"1000"}
export GRPC_CLIENT_QPS=${GRPC_CLIENT_QPS:-"0"}
export GRPC_CLIENT_QPS=$(( GRPC_CLIENT_QPS / GRPC_CLIENT_CONCURRENCY ))
export GRPC_CLIENT_CPUS=${GRPC_CLIENT_CPUS:-"1"}
export GRPC_REQUEST_SCENARIO=${GRPC_REQUEST_SCENARIO:-"complex_proto"}
export GRPC_IMAGE_NAME="${GRPC_IMAGE_NAME:-grpc_bench}"
export GRPC_GHZ_TAG="${GRPC_GHZ_TAG:-0.114.0}"
export GRPC_CLIENT="${GRPC_CLIENT:-ghz}"

# Let containers know how many CPUs they will be running on
# Additionally export other vars for further analysis script.
# export GRPC_SERVER_CPUS
# export GRPC_CLIENT_CPUS
# export GRPC_BENCHMARK_DURATION
# export GRPC_BENCHMARK_WARMUP
# export GRPC_CLIENT_CONNECTIONS
# export GRPC_CLIENT_CONCURRENCY
# export GRPC_CLIENT_QPS

wait_on_tcp50051() {
	for ((i=1;i<=10*30;i++)); do
		nc -z localhost 50051 && return 0
		sleep .1
	done
	return 1
}

# Runs the load generator for the given duration and prints its report.
# GRPC_CLIENT=ghz (default) uses ghz; GRPC_CLIENT=tonic uses the closed-loop client from tonic_client/,
# which needs several times less CPU per call and so can saturate servers ghz can't.
run_client() {
	local duration="$1"
	if [[ "${GRPC_CLIENT}" == "tonic" ]]; then
		docker run --name grpc_bench_client --rm --network=host \
			-v "${PWD}/payload:/payload:ro" \
			--cpus $GRPC_CLIENT_CPUS \
			"$GRPC_IMAGE_NAME:tonic_client-$GRPC_REQUEST_SCENARIO" \
			--cpus="${GRPC_CLIENT_CPUS}" \
			--concurrency="${GRPC_CLIENT_CONCURRENCY}" \
			--connections="${GRPC_CLIENT_CONNECTIONS}" \
			--duration "${duration}" \
			--data-file /payload/payload \
			127.0.0.1:50051
	else
		docker run --name ghz --rm --network=host -v "${PWD}/proto:/proto:ro" \
			-v "${PWD}/payload:/payload:ro" \
			--cpus $GRPC_CLIENT_CPUS \
			ghcr.io/bojand/ghz:"${GRPC_GHZ_TAG}" \
			--proto=/proto/helloworld/helloworld.proto \
			--call=helloworld.Greeter.SayHello \
			--disable-template-functions \
			--disable-template-data \
			--insecure \
			--concurrency="${GRPC_CLIENT_CONCURRENCY}" \
			--connections="${GRPC_CLIENT_CONNECTIONS}" \
			--rps="${GRPC_CLIENT_QPS}" \
			--duration "${duration}" \
			--data-file /payload/payload \
			127.0.0.1:50051
	fi
}

if [[ "${GRPC_CLIENT}" == "tonic" ]]; then
	if [[ "${GRPC_CLIENT_QPS}" != "0" ]]; then
		echo "GRPC_CLIENT=tonic doesn't support GRPC_CLIENT_QPS: it always sends calls as fast as it can."
		exit 1
	fi
	echo "==> Building the tonic client..."
	if ! (sh setup_scenario.sh $GRPC_REQUEST_SCENARIO true &&
		docker image build --quiet --file tonic_client/Dockerfile \
			--tag "$GRPC_IMAGE_NAME:tonic_client-$GRPC_REQUEST_SCENARIO" . >/dev/null); then
		echo "Building the tonic client fiascoed."
		exit 1
	fi
fi

# Loop over benchs
for benchmark in ${BENCHMARKS_TO_RUN}; do
	NAME="${benchmark##*/}"
	echo "==> Running benchmark for ${NAME}..."

	mkdir -p "${RESULTS_DIR}"

	# Setup the chosen scenario
    if ! sh setup_scenario.sh $GRPC_REQUEST_SCENARIO true; then
  		echo "Scenario setup fiascoed."
  		exit 1
	fi

	# Start the gRPC Server container
	docker run \
		--name "${NAME}" \
		--rm \
		--cpus "${GRPC_SERVER_CPUS}" \
		--memory "${GRPC_SERVER_RAM}" \
		-e GRPC_SERVER_CPUS \
		-e GRPC_SERVER_RAM \
		-p 50051:50051 \
		--detach \
		--tty \
		"$GRPC_IMAGE_NAME:${NAME}-$GRPC_REQUEST_SCENARIO" >/dev/null

	printf 'Waiting for server to come up... '
	if ! wait_on_tcp50051; then
		echo 'server unresponsive!'
		exit 1
	fi
	echo 'ready.'

	# Warm up the service
    if [[ "${GRPC_BENCHMARK_WARMUP}" != "0s" ]]; then
      	echo -n "Warming up the service for ${GRPC_BENCHMARK_WARMUP}... "
    	run_client "${GRPC_BENCHMARK_WARMUP}" > /dev/null

    	echo "done."
    else
        echo "gRPC Server Warmup skipped."
    fi

	# Actual benchmark
	echo "Benchmarking now... "

	# Start collecting stats
	./collect_stats.sh "${NAME}" "${RESULTS_DIR}" &

	# Start the gRPC Client
	run_client "${GRPC_BENCHMARK_DURATION}" >"${RESULTS_DIR}/${NAME}".report

	# Show quick summary (reqs/sec)
	cat << EOF
		done.
		Results:
		$(cat "${RESULTS_DIR}/${NAME}".report | grep "Requests/sec" | sed -E 's/^ +/    /')
EOF

	kill -INT %1 2>/dev/null
	docker container stop "${NAME}" >/dev/null
done

if sh analyze.sh $RESULTS_DIR; then
  cat ${RESULTS_DIR}/bench.params
  echo "All done."
else
  echo "Analysis fiascoed."
  ls -lha $RESULTS_DIR
  for f in $RESULTS_DIR/*; do
  	echo
  	echo
  	echo "$f"
	  cat "$f"
  done
  exit 1
fi
