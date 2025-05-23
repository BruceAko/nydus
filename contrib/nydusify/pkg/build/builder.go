// Copyright 2023 Nydus Developers. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

package build

import (
	"io"
	"os"
	"os/exec"
	"strings"

	"github.com/sirupsen/logrus"
)

type BuilderOption struct {
	ParentBootstrapPath string
	ChunkDict           string
	BootstrapPath       string
	RootfsPath          string
	BackendType         string
	BackendConfig       string
	WhiteoutSpec        string
	OutputJSONPath      string
	PrefetchPatterns    string
	// A regular file or fifo into which commands nydus-image to dump contents.
	BlobPath     string
	AlignedChunk bool
	Compressor   string
	ChunkSize    string
	FsVersion    string
}

type CompactOption struct {
	ChunkDict           string
	BootstrapPath       string
	OutputBootstrapPath string
	BackendType         string
	BackendConfigPath   string
	OutputJSONPath      string

	MinUsedRatio    string
	CompactBlobSize string
	MaxCompactSize  string
	LayersToCompact string
	BlobsDir        string
}

type GenerateOption struct {
	BootstrapPaths         []string
	DatabasePath           string
	ChunkdictBootstrapPath string
	OutputPath             string
}

type Builder struct {
	binaryPath string
	stdout     io.Writer
	stderr     io.Writer
}

func NewBuilder(binaryPath string) *Builder {
	return &Builder{
		binaryPath: binaryPath,
		stdout:     os.Stdout,
		stderr:     os.Stderr,
	}
}

func (builder *Builder) run(args []string, prefetchPatterns string) error {
	logrus.Debugf("\tCommand: %s %s", builder.binaryPath, strings.Join(args[:], " "))

	cmd := exec.Command(builder.binaryPath, args...)
	cmd.Stdout = builder.stdout
	cmd.Stderr = builder.stderr
	cmd.Stdin = strings.NewReader(prefetchPatterns)

	if err := cmd.Run(); err != nil {
		logrus.WithError(err).Errorf("fail to run %v %+v", builder.binaryPath, args)
		return err
	}

	return nil
}

func (builder *Builder) Compact(option CompactOption) error {
	args := []string{
		"compact",
		"--bootstrap", option.BootstrapPath,
		"--blob-dir", option.BlobsDir,
		"--min-used-ratio", option.MinUsedRatio,
		"--compact-blob-size", option.CompactBlobSize,
		"--max-compact-size", option.MaxCompactSize,
		"--layers-to-compact", option.LayersToCompact,
		"--backend-type", option.BackendType,
		"--backend-config-file", option.BackendConfigPath,
		"--log-level", "info",
		"--output-json", option.OutputJSONPath,
	}
	if option.OutputBootstrapPath != "" {
		args = append(args, "--output-bootstrap", option.OutputBootstrapPath)
	}
	if option.ChunkDict != "" {
		args = append(args, "--chunk-dict", option.ChunkDict)
	}
	return builder.run(args, "")
}

// Run exec nydus-image CLI to build layer
func (builder *Builder) Run(option BuilderOption) error {
	var args []string
	if option.ParentBootstrapPath == "" {
		args = []string{
			"create",
		}
	} else {
		args = []string{
			"create",
			"--parent-bootstrap",
			option.ParentBootstrapPath,
		}
	}
	if option.AlignedChunk {
		args = append(args, "--aligned-chunk")
	}
	if option.ChunkDict != "" {
		args = append(args, "--chunk-dict", option.ChunkDict)
	}

	args = append(
		args,
		"--bootstrap",
		option.BootstrapPath,
		"--log-level",
		"warn",
		"--whiteout-spec",
		option.WhiteoutSpec,
		"--output-json",
		option.OutputJSONPath,
		"--blob",
		option.BlobPath,
		"--fs-version",
		option.FsVersion,
	)

	if option.Compressor != "" {
		args = append(args, "--compressor", option.Compressor)
	}

	if len(option.PrefetchPatterns) > 0 {
		args = append(args, "--prefetch-policy", "fs")
	}

	if option.ChunkSize != "" {
		args = append(args, "--chunk-size", option.ChunkSize)
	}

	args = append(args, option.RootfsPath)

	return builder.run(args, option.PrefetchPatterns)
}

// Generate calls `nydus-image chunkdict generate` to get chunkdict
func (builder *Builder) Generate(option GenerateOption) error {
	logrus.Infof("Invoking 'nydus-image chunkdict generate' command")
	args := []string{
		"chunkdict",
		"generate",
		"--log-level",
		"warn",
		"--bootstrap",
		option.ChunkdictBootstrapPath,
		"--database",
		option.DatabasePath,
		"--output-json",
		option.OutputPath,
	}
	args = append(args, option.BootstrapPaths...)

	return builder.run(args, "")
}
