// Base class fixture.
#import <Foundation/Foundation.h>

@interface Animal : NSObject
@property (nonatomic, copy) NSString *name;
- (instancetype)initWithName:(NSString *)name;
- (NSString *)describe;
@end
