#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <MetalPerformanceShadersGraph/MetalPerformanceShadersGraph.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <vector>

struct SarunWeight {
    const uint8_t *data;
    size_t len;
};

struct SarunRealPlksr {
    MPSGraph *graph;
    MPSGraphTensor *input;
    MPSGraphTensor *output;
    id<MTLDevice> device;
    id<MTLCommandQueue> queue;
};

static void set_error(char *error, size_t error_len, NSString *message) {
    if (error == nullptr || error_len == 0) {
        return;
    }
    const char *utf8 = message.UTF8String;
    if (utf8 == nullptr) {
        utf8 = "unknown RealPLKSR error";
    }
    size_t len = std::min(error_len - 1, strlen(utf8));
    memcpy(error, utf8, len);
    error[len] = '\0';
}

static MPSShape *shape(std::initializer_list<NSInteger> dimensions) {
    NSMutableArray<NSNumber *> *result =
        [NSMutableArray arrayWithCapacity:dimensions.size()];
    for (NSInteger dimension : dimensions) {
        [result addObject:@(dimension)];
    }
    return result;
}

class WeightCursor {
  public:
    WeightCursor(MPSGraph *graph, const SarunWeight *weights, size_t count)
        : graph_(graph), weights_(weights), count_(count) {}

    MPSGraphTensor *next(std::initializer_list<NSInteger> dimensions) {
        if (index_ >= count_) {
            @throw [NSException exceptionWithName:@"SarunRealPLKSRWeights"
                                           reason:@"checkpoint ended before the fixed graph"
                                         userInfo:nil];
        }
        size_t elements = 1;
        for (NSInteger dimension : dimensions) {
            elements *= static_cast<size_t>(dimension);
        }
        const SarunWeight &weight = weights_[index_++];
        if (weight.len != elements * sizeof(float)) {
            @throw [NSException
                exceptionWithName:@"SarunRealPLKSRWeights"
                             reason:[NSString
                                        stringWithFormat:
                                            @"tensor %zu has %zu bytes; expected %zu",
                                            index_ - 1, weight.len,
                                            elements * sizeof(float)]
                           userInfo:nil];
        }
        NSData *data = [NSData dataWithBytes:weight.data length:weight.len];
        return [graph_ constantWithData:data
                                  shape:shape(dimensions)
                               dataType:MPSDataTypeFloat32];
    }

    size_t index() const { return index_; }

  private:
    MPSGraph *graph_;
    const SarunWeight *weights_;
    size_t count_;
    size_t index_ = 0;
};

static MPSGraphTensor *add(MPSGraph *graph, MPSGraphTensor *left,
                           MPSGraphTensor *right) {
    return [graph additionWithPrimaryTensor:left secondaryTensor:right name:nil];
}

static MPSGraphTensor *multiply(MPSGraph *graph, MPSGraphTensor *left,
                                MPSGraphTensor *right) {
    return [graph multiplicationWithPrimaryTensor:left
                                  secondaryTensor:right
                                             name:nil];
}

static MPSGraphTensor *convolution(MPSGraph *graph, WeightCursor &weights,
                                   MPSGraphTensor *source, NSInteger out_channels,
                                   NSInteger in_channels, NSInteger kernel) {
    MPSGraphTensor *weight =
        weights.next({out_channels, in_channels, kernel, kernel});
    MPSGraphTensor *bias = weights.next({1, 1, 1, out_channels});
    NSUInteger padding = static_cast<NSUInteger>(kernel / 2);
    MPSGraphConvolution2DOpDescriptor *descriptor =
        [MPSGraphConvolution2DOpDescriptor
            descriptorWithStrideInX:1
                          strideInY:1
                    dilationRateInX:1
                    dilationRateInY:1
                             groups:1
                        paddingLeft:padding
                       paddingRight:padding
                         paddingTop:padding
                      paddingBottom:padding
                       paddingStyle:MPSGraphPaddingStyleExplicit
                         dataLayout:MPSGraphTensorNamedDataLayoutNHWC
                      weightsLayout:MPSGraphTensorNamedDataLayoutOIHW];
    MPSGraphTensor *result =
        [graph convolution2DWithSourceTensor:source
                              weightsTensor:weight
                                 descriptor:descriptor
                                       name:nil];
    return add(graph, result, bias);
}

static MPSGraphTensor *mish(MPSGraph *graph, MPSGraphTensor *source) {
    // Stable softplus: max(x, 0) + log(1 + exp(-abs(x))).
    MPSGraphTensor *zero =
        [graph constantWithScalar:0.0 dataType:MPSDataTypeFloat32];
    MPSGraphTensor *one =
        [graph constantWithScalar:1.0 dataType:MPSDataTypeFloat32];
    MPSGraphTensor *positive =
        [graph maximumWithPrimaryTensor:source secondaryTensor:zero name:nil];
    MPSGraphTensor *absolute = [graph absoluteWithTensor:source name:nil];
    MPSGraphTensor *negative_absolute =
        [graph negativeWithTensor:absolute name:nil];
    MPSGraphTensor *exponential =
        [graph exponentWithTensor:negative_absolute name:nil];
    MPSGraphTensor *logarithm =
        [graph logarithmWithTensor:add(graph, one, exponential) name:nil];
    MPSGraphTensor *softplus = add(graph, positive, logarithm);
    MPSGraphTensor *activation = [graph tanhWithTensor:softplus name:nil];
    return multiply(graph, source, activation);
}

static MPSGraphTensor *group_norm(MPSGraph *graph, WeightCursor &weights,
                                  MPSGraphTensor *source) {
    // PyTorch GroupNorm(4, 64): each group spans all pixels and 16 channels.
    // One inferred dimension keeps the graph independent of image width/height.
    MPSGraphTensor *grouped =
        [graph reshapeTensor:source withShape:shape({1, -1, 4, 16}) name:nil];
    NSArray<NSNumber *> *axes = @[ @1, @3 ];
    MPSGraphTensor *mean = [graph meanOfTensor:grouped axes:axes name:nil];
    MPSGraphTensor *variance =
        [graph varianceOfTensor:grouped meanTensor:mean axes:axes name:nil];
    MPSGraphTensor *gamma = weights.next({1, 1, 4, 16});
    MPSGraphTensor *beta = weights.next({1, 1, 4, 16});
    MPSGraphTensor *normalized =
        [graph normalizationWithTensor:grouped
                            meanTensor:mean
                        varianceTensor:variance
                           gammaTensor:gamma
                            betaTensor:beta
                               epsilon:1.0e-5f
                                  name:nil];
    MPSGraphTensor *original_shape = [graph shapeOfTensor:source name:nil];
    return [graph reshapeTensor:normalized
                withShapeTensor:original_shape
                           name:nil];
}

extern "C" void *sarun_realplksr_create(const SarunWeight *raw_weights,
                                         size_t count, char *error,
                                         size_t error_len) {
    @autoreleasepool {
        @try {
            if (count != 340) {
                set_error(error, error_len,
                          [NSString stringWithFormat:
                                        @"fixed RealPLKSR graph needs 340 tensors; got %zu",
                                        count]);
                return nullptr;
            }
            id<MTLDevice> device = MTLCreateSystemDefaultDevice();
            if (device == nil) {
                set_error(error, error_len, @"no Metal device is available");
                return nullptr;
            }
            id<MTLCommandQueue> queue = [device newCommandQueue];
            if (queue == nil) {
                set_error(error, error_len,
                          @"could not create a Metal command queue");
                return nullptr;
            }
            MPSGraph *graph = [MPSGraph new];
            WeightCursor weights(graph, raw_weights, count);
            MPSGraphTensor *input =
                [graph placeholderWithShape:shape({1, -1, -1, 3})
                                   dataType:MPSDataTypeFloat32
                                       name:@"rgb"];
            MPSGraphTensor *value =
                convolution(graph, weights, input, 64, 3, 3);
            for (NSInteger block = 0; block < 28; ++block) {
                MPSGraphTensor *skip = value;
                value = convolution(graph, weights, value, 128, 64, 3);
                value = mish(graph, value);
                value = convolution(graph, weights, value, 64, 128, 3);

                MPSGraphTensor *large =
                    [graph sliceTensor:value
                             dimension:3
                                 start:0
                                length:16
                                  name:nil];
                MPSGraphTensor *rest =
                    [graph sliceTensor:value
                             dimension:3
                                 start:16
                                length:48
                                  name:nil];
                large = convolution(graph, weights, large, 16, 16, 17);
                value = [graph concatTensors:@[ large, rest ]
                                   dimension:3
                                        name:nil];

                MPSGraphTensor *attention =
                    convolution(graph, weights, value, 64, 64, 3);
                attention = [graph sigmoidWithTensor:attention name:nil];
                value = multiply(graph, value, attention);
                value = convolution(graph, weights, value, 64, 64, 1);
                value = group_norm(graph, weights, value);
                value = add(graph, value, skip);
            }
            value = convolution(graph, weights, value, 48, 64, 3);
            // torch.repeat_interleave(input, 16, dim=channel), not a plain
            // channel tile: [R,G,B] must become [R×16,G×16,B×16].
            MPSGraphTensor *repeat_source =
                [graph reshapeTensor:input withShape:shape({1, -1, 3, 1}) name:nil];
            MPSGraphTensor *repeated =
                [graph tileTensor:repeat_source
                   withMultiplier:shape({1, 1, 1, 16})
                             name:nil];
            MPSGraphTensor *input_shape = [graph shapeOfTensor:input name:nil];
            MPSGraphTensor *spatial_shape =
                [graph sliceTensor:input_shape
                         dimension:0
                             start:0
                            length:3
                              name:nil];
            MPSGraphTensor *channels =
                [graph constantWithScalar:48
                                    shape:shape({1})
                                 dataType:MPSDataTypeInt32];
            MPSGraphTensor *repeat_shape =
                [graph concatTensors:@[ spatial_shape, channels ]
                           dimension:0
                                name:nil];
            repeated =
                [graph reshapeTensor:repeated
                    withShapeTensor:repeat_shape
                               name:nil];
            value = add(graph, value, repeated);
            MPSGraphTensor *output =
                [graph depthToSpace2DTensor:value
                                  widthAxis:2
                                 heightAxis:1
                                  depthAxis:3
                                  blockSize:4
                       usePixelShuffleOrder:YES
                                       name:@"enhanced_rgb"];
            if (weights.index() != count) {
                set_error(error, error_len,
                          [NSString stringWithFormat:
                                        @"fixed graph consumed %zu of %zu tensors",
                                        weights.index(), count]);
                return nullptr;
            }
            SarunRealPlksr *runtime = new SarunRealPlksr{
                graph,
                input,
                output,
                device,
                queue,
            };
            return runtime;
        } @catch (NSException *exception) {
            set_error(error, error_len, exception.reason);
            return nullptr;
        }
    }
}

extern "C" bool sarun_realplksr_run(void *opaque, const float *input,
                                    uint32_t width, uint32_t height,
                                    float *output, char *error,
                                    size_t error_len) {
    @autoreleasepool {
        @try {
            if (opaque == nullptr || input == nullptr || output == nullptr) {
                set_error(error, error_len, @"invalid RealPLKSR inference buffer");
                return false;
            }
            SarunRealPlksr *runtime = static_cast<SarunRealPlksr *>(opaque);
            size_t input_len =
                static_cast<size_t>(width) * static_cast<size_t>(height) * 3 *
                sizeof(float);
            NSData *bytes = [NSData dataWithBytesNoCopy:const_cast<float *>(input)
                                                length:input_len
                                          freeWhenDone:NO];
            MPSGraphTensorData *input_data =
                [[MPSGraphTensorData alloc]
                    initWithDevice:[MPSGraphDevice
                                       deviceWithMTLDevice:runtime->device]
                              data:bytes
                             shape:shape({1, height, width, 3})
                          dataType:MPSDataTypeFloat32];
            NSDictionary<MPSGraphTensor *, MPSGraphTensorData *> *results =
                [runtime->graph runWithMTLCommandQueue:runtime->queue
                                                feeds:@{runtime->input : input_data}
                                        targetTensors:@[ runtime->output ]
                                     targetOperations:nil];
            MPSGraphTensorData *output_data = results[runtime->output];
            if (output_data == nil) {
                set_error(error, error_len, @"Metal returned no output tensor");
                return false;
            }
            [[output_data mpsndarray] readBytes:output strideBytes:nil];
            return true;
        } @catch (NSException *exception) {
            set_error(error, error_len, exception.reason);
            return false;
        }
    }
}

extern "C" void sarun_realplksr_destroy(void *opaque) {
    delete static_cast<SarunRealPlksr *>(opaque);
}
