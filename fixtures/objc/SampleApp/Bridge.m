// Fixture for issue #449: ObjC → Swift bridged call. `ClassC` is a Swift class
// (see fixtures/swift/SampleApp/ClassC.swift); without the generated -Swift.h
// header clang cannot resolve the selector, so the visitor must synthesize the
// reference from the static receiver + selector, and scip-reader must convert
// the def-less ref into an UnresolvedCall for the daemon to resolve.
#import <Foundation/Foundation.h>

@interface Bridge : NSObject
- (void)setUp;
@end

@implementation Bridge

- (void)setUp {
    [ClassC registerEnvironments];
}

@end
